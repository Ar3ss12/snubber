//! SPEC-02: Frame Parser Differential Fuzzer.
//!
//! The `SNUB`-framed parser processes a noise stream (magic splits,
//! truncations, alien payload buffer) plus single-bit flips of a valid
//! 3-frame digest stream: never panics, never promotes a torn digest,
//! the drain always terminates on EOF. Native runs 1 MB / 1056 mutants /
//! 2000 aliens; Miri runs a 16 KB / step-16 slice (same 0-UB verdict).

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

#[inline]
fn drain_of(bytes: Vec<u8>) -> snubber::PipeDrain {
    snubber::PipeDrain::drain_sync(std::io::Cursor::new(bytes))
}

/// Corpus sizes: native runs the full 1 MB + 1056 mutants + 2000 aliens;
/// Miri (MIR interpreter, ~1000x slower per byte) runs a 16 KB slice with
/// every 16th mutant. A provenance/alignment/init UB fires on the first
/// bytes, not on megabyte 1 — the slice gives the same 0-UB verdict.
#[cfg(miri)]
const NOISE_LEN: usize = 16_384;
#[cfg(not(miri))]
const NOISE_LEN: usize = 1_000_000;
#[cfg(miri)]
const MAGIC_SMEARS: usize = 8;
#[cfg(not(miri))]
const MAGIC_SMEARS: usize = 200;
#[cfg(miri)]
const MUTANT_STEP: usize = 16;
#[cfg(not(miri))]
const MUTANT_STEP: usize = 1;
#[cfg(miri)]
const ALIEN_FRAMES: usize = 64;
#[cfg(not(miri))]
const ALIEN_FRAMES: usize = 2000;

#[test]
fn frame_parser_fuzzer() {
    use snubber::{StateDigest, OP_STATE};

    // A. Noise stream corpus: random bytes with smeared magic + torn pads.
    let mut rng = Xor64(0x9E37_79B9_7F4A_7C15);
    let mut noise_stream = Vec::with_capacity(NOISE_LEN);
    for _ in 0..NOISE_LEN {
        noise_stream.push(rng.next_byte());
    }
    for i in 0..MAGIC_SMEARS {
        let pos = (rng.next() as usize) % (noise_stream.len() - 44);
        noise_stream[pos..pos + 4].copy_from_slice(b"SNUB");
        if i % 3 == 0 {
            // Torn body at the magic: truncate tail, re-pad with zeros so
            // the total length stays NOISE_LEN and the drain still hits EOF.
            noise_stream.truncate(pos + 4 + (rng.next() as usize % 39));
            noise_stream.extend(std::iter::repeat_n(0u8, NOISE_LEN - noise_stream.len()));
        }
    }
    assert_eq!(noise_stream.len(), NOISE_LEN);
    let d = drain_of(noise_stream);
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
    let mut checked = 0u32;
    for pos in 0..wire.len() {
        for bit in 0..8u8 {
            // Miri slice: every 16th mutant covers the same byte/bit
            // classes (magic, len, CRC, pad) at 1/16th of the cost.
            if (pos as u32 * 8 + bit as u32) % MUTANT_STEP as u32 != 0 {
                continue;
            }
            checked += 1;
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

    // C. Unrecognized frames stream: valid framing, alien bodies — drain survives,
    // never promotes a state digest.
    let mut alien_payload_buffer = Vec::new();
    let mut rng2 = Xor64(0x1234_5678_9ABC_DEF0);
    for _ in 0..ALIEN_FRAMES {
        alien_payload_buffer.extend_from_slice(b"SNUB");
        for _ in 0..40 {
            alien_payload_buffer.push(rng2.next_byte());
        }
    }
    let d = drain_of(alien_payload_buffer);
    assert!(
        d.last_state().is_none(),
        "unrecognized frames stream never promotes"
    );

    println!(
        "\n[FUZZ]: corpus={NOISE_LEN}B bitmutants={checked}(rejected={rejected} identical={identical}) alien={ALIEN_FRAMES}frames corrupt=0(control=1) panics=0"
    );
}
