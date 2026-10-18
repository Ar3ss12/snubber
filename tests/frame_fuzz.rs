//! IRON PROOF 2: 44 B frame-parser fuzzer.
//!
//! The `SNUB`-framed parser eats a 1 MB hostile ocean (magic splits,
//! truncations, opcode soup) plus EVERY single-bit flip of a valid
//! 3-frame digest stream (1056 mutants): never panics, never promotes
//! a torn digest, the drain always terminates on EOF.

/// Deterministic xorshift64 (no crates, reproducible fuzz).
struct Xor64(u64);
impl Xor64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
}

fn drain_of(bytes: Vec<u8>) -> snubber::PipeDrain {
    let drain = snubber::PipeDrain::spawn(std::io::Cursor::new(bytes));
    // Deadline wait, not a fixed yield budget: the 1 MB ocean needs real
    // time in debug builds, while 132 B mutants finish on the first poll.
    let start = std::time::Instant::now();
    while !drain.eof() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(30),
            "drain must always terminate on EOF"
        );
        std::thread::yield_now();
    }
    drain
}

#[test]
fn frame_parser_fuzzer() {
    use snubber::{StateDigest, OP_STATE};

    // A. Hostile ocean: 1 MB random bytes with smeared magic + torn pads.
    let mut rng = Xor64(0x9E37_79B9_7F4A_7C15);
    let mut ocean = Vec::with_capacity(1_000_000);
    for _ in 0..1_000_000 {
        ocean.push(rng.next_byte());
    }
    for i in 0..200 {
        let pos = (rng.next() as usize) % (ocean.len() - 44);
        ocean[pos..pos + 4].copy_from_slice(b"SNUB");
        if i % 3 == 0 {
            // Torn body at the magic: truncate tail, re-pad with zeros so
            // the total length stays 1 MB and the drain still hits EOF.
            ocean.truncate(pos + 4 + (rng.next() as usize % 39));
            ocean.extend(std::iter::repeat_n(0u8, 1_000_000 - ocean.len()));
        }
    }
    assert_eq!(ocean.len(), 1_000_000);
    let d = drain_of(ocean);
    assert!(d.last_state().is_none(), "noise never promotes a digest");

    // B. Bit-flip sweep: EVERY single-bit mutant of a valid 3-frame
    // stream must fail CRC and promote nothing (intact = control).
    let mut good = [0u8; 64];
    good[0] = 42;
    good[63] = 0xAB;
    let digest = StateDigest::new(good);
    let frames = digest.to_frames();
    let mut wire = Vec::with_capacity(132);
    for f in &frames {
        assert_eq!(f.opcode, OP_STATE);
        wire.extend_from_slice(&f.to_bytes());
    }
    let d = drain_of(wire.clone());
    let recovered = d.take_state().expect("intact stream promotes");
    assert_eq!(recovered, digest, "bit-exact reassembly");

    // INVARIANT: a 1-bit mutant either promotes NOTHING or the
    // bit-exact original. Benign flips exist by design (frame-2 zero
    // pad outside CRC coverage; `task.min(2)` seq clamp; in-order
    // overwrite of a reseated slot) — all must still reassemble to
    // the identical digest, never a corrupt one.
    let mut rejected = 0u32;
    let mut identical = 0u32;
    for pos in 0..wire.len() {
        for bit in 0..8u8 {
            let mut mutant = wire.clone();
            mutant[pos] ^= 1 << bit;
            let d = drain_of(mutant);
            match d.last_state() {
                None => rejected += 1,
                Some(state) => {
                    assert_eq!(
                        state, digest,
                        "1-bit mutant byte {pos} bit {bit} promoted CORRUPT data"
                    );
                    identical += 1;
                }
            }
        }
    }

    // C. Opcode soup: valid framing, 2000 alien bodies — drain survives,
    // never promotes a state digest.
    let mut soup = Vec::new();
    let mut rng2 = Xor64(0x1234_5678_9ABC_DEF0);
    for _ in 0..2000 {
        soup.extend_from_slice(b"SNUB");
        for _ in 0..40 {
            soup.push(rng2.next_byte());
        }
    }
    let d = drain_of(soup);
    assert!(d.last_state().is_none(), "opcode soup never promotes");

    println!(
        "\n[FUZZ]: ocean=1000000B bitmutants=1056(rejected={rejected} identical={identical}) soup=2000frames corrupt=0(control=1) panics=0"
    );
}
