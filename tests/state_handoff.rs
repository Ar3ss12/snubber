//! Phase 3 test
use snubber::STATE_LEN;
#[test]
fn len_is_cache_line() {
    assert_eq!(STATE_LEN, 64);
}
#[test]
fn digest_42_survives() {
    use snubber::{StateDigest, OP_STATE};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    let mut bytes = [0u8; 64];
    bytes[0] = 42;
    let digest = StateDigest::new(bytes);
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    let addr = listener.local_addr().expect("addr");
    let wh = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("c");
        w.write_all(b"noise\n").unwrap();
        for f in digest.to_frames() {
            assert_eq!(f.opcode, OP_STATE);
            w.write_all(&f.to_bytes()).unwrap();
        }
        w.flush().unwrap();
    });
    let (reader, _) = listener.accept().expect("a");
    let drain = snubber::PipeDrain::spawn(reader);
    wh.join().expect("w");
    for _ in 0..5000 {
        if drain.eof() {
            break;
        }
        std::thread::yield_now();
    }
    let heir = drain.take_state().expect("kept");
    assert_eq!(heir.bytes[0], 42);
    assert_eq!(heir, digest);
    assert!(drain.take_armed().is_none());
    println!("\n[HANDOFF]: published=64 kept=64 mark=42 heir=42 torn=0 poison=0");
}
#[test]
fn torn_half_dropped_last_whole_kept() {
    use snubber::StateDigest;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    let mut g = [0u8; 64];
    g[0] = 7;
    let grandpa = StateDigest::new(g);
    let mut t = [0u8; 64];
    t[0] = 9;
    let torn = StateDigest::new(t);
    let listener = TcpListener::bind("127.0.0.1:0").expect("l");
    let addr = listener.local_addr().expect("a");
    let wh = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("c");
        for f in grandpa.to_frames() {
            w.write_all(&f.to_bytes()).unwrap();
        }
        let fr = torn.to_frames();
        w.write_all(&fr[0].to_bytes()).unwrap();
        w.write_all(&fr[1].to_bytes()).unwrap();
        w.flush().unwrap();
    });
    let (reader, _) = listener.accept().expect("a");
    let drain = snubber::PipeDrain::spawn(reader);
    wh.join().expect("w");
    for _ in 0..5000 {
        if drain.eof() {
            break;
        }
        std::thread::yield_now();
    }
    let heir = drain.last_state().expect("grandpa");
    assert_eq!(heir.bytes[0], 7);
}
#[test]
fn bitten_frame_crc_dropped() {
    use snubber::StateDigest;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    let mut b = [0u8; 64];
    b[0] = 11;
    let digest = StateDigest::new(b);
    let listener = TcpListener::bind("127.0.0.1:0").expect("l");
    let addr = listener.local_addr().expect("a");
    let wh = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("c");
        let mut fr = digest.to_frames();
        fr[1].payload[5] ^= 0xFF;
        for f in fr {
            w.write_all(&f.to_bytes()).unwrap();
        }
        w.flush().unwrap();
    });
    let (reader, _) = listener.accept().expect("a");
    let drain = snubber::PipeDrain::spawn(reader);
    wh.join().expect("w");
    for _ in 0..5000 {
        if drain.eof() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(drain.last_state().is_none());
}

#[test]
fn mismatched_generation_chunks_rejected_by_full_crc() {
    use snubber::StateDigest;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    let mut a_bytes = [0xAAu8; 64];
    a_bytes[0] = 1;
    let digest_a = StateDigest::new(a_bytes);

    let mut b_bytes = [0xBBu8; 64];
    b_bytes[0] = 2;
    let digest_b = StateDigest::new(b_bytes);

    let listener = TcpListener::bind("127.0.0.1:0").expect("l");
    let addr = listener.local_addr().expect("a");

    let wh = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("c");
        let frames_a = digest_a.to_frames();
        let frames_b = digest_b.to_frames();

        // Send frames 0 & 1 from revision A
        w.write_all(&frames_a[0].to_bytes()).unwrap();
        w.write_all(&frames_a[1].to_bytes()).unwrap();

        // Send frame 2 from revision B (simulating fragmented/interleaved buffer state)
        w.write_all(&frames_b[2].to_bytes()).unwrap();
        w.flush().unwrap();
    });

    let (reader, _) = listener.accept().expect("a");
    let drain = snubber::PipeDrain::spawn(reader);
    wh.join().expect("w");

    for _ in 0..5000 {
        if drain.eof() {
            break;
        }
        std::thread::yield_now();
    }

    // Full CRC mismatch MUST reject the Franken-state
    assert!(
        drain.last_state().is_none(),
        "Mismatched chunks from different revisions must NOT be promoted to valid state"
    );
}
