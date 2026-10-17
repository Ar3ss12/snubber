//! Path-1 spine + Path-2 scalpel: generation bump cuts zombies,
//! pipe EOF fires the last-armed token. Dumb supervisor, honest counters.

use snubber::{
    DeathCause, EnterError, GenerationClock, PipeDrain, ProcessToken, SnubberEvent, SnubberNode,
    TransactionHeader, OP_COMMIT,
};

#[test]
fn supervisor_restart_cuts_zombies() {
    // gen0 lives to epoch 1420, then dies. gen1 re-handshakes at epoch 1;
    // zombie packets from the dead lineage are cut even with HIGHER epoch.
    let node = SnubberNode::builder("super/lineage").build();
    let mut clock = GenerationClock::new();
    assert_eq!(clock.generation(), 1);
    let g0 = clock.generation();
    node.enter_with_generation(g0, 1420, 1, || {})
        .expect("gen1 live")
        .commit();
    // Death: bump generation, reset epoch.
    let g1 = clock.restart();
    assert_eq!(g1, 2);
    let e1 = clock.alloc_epoch();
    assert_eq!(e1, 1);
    node.enter_with_generation(g1, e1, 2, || {})
        .expect("gen2 re-handshake")
        .commit();
    // Zombie from the dead lineage — higher epoch, old generation: cut.
    match node.enter_with_generation(g0, 9999, 3, || {}) {
        Err(EnterError::StaleEpoch) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("zombie must be cut"),
    }
    let s = node.stats();
    assert_eq!(s.total_started, 2, "only live generations armed");
    assert_eq!(s.stale_epochs_rejected, 1, "zombie counted");
    assert_eq!(s.committed, 2);
}

#[test]
fn death_emit_is_aborted_and_terminal() {
    let header = TransactionHeader::with_generation(2, 1, 7);
    let ev = snubber::supervisor::death_event(header, DeathCause::AbortUnwind);
    assert!(ev.is_terminal(), "death is terminal");
    match ev {
        SnubberEvent::Aborted { header: h, reason } => {
            assert_eq!(h, header);
            assert_eq!(reason, "process died");
        }
        _ => panic!("death must emit Aborted"),
    }
    // Temporary honesty check done — v0.4 has its own kind now.
    let hang = snubber::supervisor::death_event(header, DeathCause::HangTimeout);
    match hang {
        SnubberEvent::Aborted { reason, .. } => {
            assert_eq!(reason, "process hung past lease");
        }
        _ => unreachable!(),
    }
}

#[test]
fn token_roundtrip_is_44_bytes_framed() {
    let mut payload = [0u8; 28];
    payload[0] = 0xAB;
    payload[27] = 0xCD;
    let t = ProcessToken::new(0x1122_3344_5566_7788, 0xAABB_CCDD, payload);
    let wire = t.to_bytes();
    assert_eq!(wire.len(), 44);
    assert_eq!(&wire[0..4], b"SNUB");
    let back = ProcessToken::from_body_bytes(wire[4..44].try_into().unwrap());
    assert_eq!(back, t);
    let c = ProcessToken::commit(99);
    assert_eq!(c.opcode, OP_COMMIT);
    assert_eq!(c.task, 99);
}

#[test]
fn drain_filters_println_noise_and_recovers_token() {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    let addr = listener.local_addr().expect("addr");

    let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
    let writer_h = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("connect");
        // Spam arbitrary stdout println noise!
        w.write_all(b"Connecting to database...\n").unwrap();
        w.write_all(b"[INFO] GUI initialized in 12ms\n").unwrap();
        w.write_all(b"User clicked button F6\n").unwrap();

        // Emit framed token (opcode 42, NOT 1 which is OP_HEARTBEAT!)
        let armed = ProcessToken::simple(555, 42, 77);
        w.write_all(&armed.to_bytes()).unwrap();

        // More noise!
        w.write_all(b"Random frame delta: 5.065ms\n").unwrap();

        // Close stream (EOF)
        drop(w);
        let _ = tx_done.send(());
    });

    let (reader, _) = listener.accept().expect("accept");
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let drain = PipeDrain::spawn(reader);
    writer_h.join().expect("writer done");
    rx_done.recv_timeout(Duration::from_secs(10)).expect("eof");

    let mut spins = 0u32;
    while !drain.eof() && spins < 10_000 {
        std::thread::sleep(Duration::from_millis(1));
        spins += 1;
    }

    let token = drain.take_armed().expect("token must survive text noise");
    assert_eq!(token.task, 555);
    assert_eq!(token.opcode, 42);
    assert_eq!(token.payload[0], 77);
}

#[test]
fn drain_holds_last_armed_and_eof_fires_it() {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;
    // Anonymous-pipe stand-in on stable `std`: loopback TCP is a byte
    // stream with kernel buffering + EOF-on-close — same contract as a
    // `std::process` pipe (drain in real time, EOF means death).
    // Proves the CHOKE fix: reader drains while writer hammers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    let addr = listener.local_addr().expect("addr");
    let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
    let writer_h = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("connect");
        // Arm token (opcode 7) then 2000 commits — drain must keep up,
        // kernel buffer must never choke the writer.
        let mut payload = [0u8; 28];
        payload[0] = 0xEE;
        let armed = ProcessToken::new(4242, 7, payload);
        w.write_all(&armed.to_bytes()).expect("arm write");
        for i in 0u64..2000 {
            let c = ProcessToken::commit(i);
            w.write_all(&c.to_bytes()).expect("commit write");
        }
        // Re-arm one live token, then die WITHOUT commit (abort mid-flight).
        let live = ProcessToken::new(7777, 9, [0xAA; 28]);
        w.write_all(&live.to_bytes()).expect("live write");
        drop(w); // EOF — like abort()/SIGKILL closing the pipe.
        let _ = tx_done.send(());
    });
    let (reader, _) = listener.accept().expect("accept");
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let drain = PipeDrain::spawn(reader);
    writer_h.join().expect("writer done");
    rx_done.recv_timeout(Duration::from_secs(10)).expect("eof");
    // Spin until the drain observes EOF (reader thread finished).
    let mut spins = 0u32;
    while !drain.eof() && spins < 10_000 {
        std::thread::sleep(Duration::from_millis(1));
        spins += 1;
    }
    assert!(drain.eof(), "drain must observe EOF after death");
    // Last-armed token survived: the 7777/9 live one (commits cleared).
    let fired = drain.take_armed().expect("un-settled token fires");
    assert_eq!(fired.task, 7777);
    assert_eq!(fired.opcode, 9);
    assert_eq!(fired.payload, [0xAA; 28]);
    // Second take = empty (exactly-once).
    assert!(drain.take_armed().is_none(), "exactly-once take");
}

#[test]
fn hang_timeout_fires_on_silence_and_restarts() {
    use snubber::OP_HEARTBEAT;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;
    // Worker arms a token, then freezes mid-flight (no commit, no traffic).
    // Supervisor lease = 50 ms: silence past lease = HangTimeout → reap →
    // rollback by token → generation bump. Pipe stays OPEN (frozen, not
    // dead) — `eof()` is false, `expired()` is the verdict.
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    let addr = listener.local_addr().expect("addr");
    let writer_h = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("connect");
        let live = ProcessToken::new(31337, 11, [0xBB; 28]);
        w.write_all(&live.to_bytes()).expect("arm");
        // Heartbeat once (proves liveness), then freeze like a dead driver.
        let hb = ProcessToken::new(0, OP_HEARTBEAT, [0; 28]);
        w.write_all(&hb.to_bytes()).expect("heartbeat");
        std::thread::sleep(Duration::from_millis(500)); // frozen, pipe open
        drop(w);
    });
    let (reader, _) = listener.accept().expect("accept");
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let drain = PipeDrain::spawn(reader);
    // Wait until the armed token lands (traffic = liveness).
    let mut spins = 0u32;
    while drain.idle_ms() == 0 && spins < 10_000 {
        std::thread::sleep(Duration::from_millis(1));
        spins += 1;
    }
    assert!(drain.idle_ms() < 500, "traffic seen, clock running");
    assert!(!drain.eof(), "frozen worker keeps the pipe open");
    assert!(!drain.expired(50_000), "fresh traffic is not expired");
    // Lease 50 ms: sleep past it while the worker is frozen.
    std::thread::sleep(Duration::from_millis(120));
    assert!(
        drain.expired(50),
        "silence past lease = HangTimeout, idle={}",
        drain.idle_ms()
    );
    // Execution: take token → rollback → restart (what supervisor does).
    static FIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let armed = drain.take_armed().expect("frozen mid-flight token");
    assert_eq!(armed.task, 31337);
    assert_eq!(armed.opcode, 11);
    FIRED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut clock = GenerationClock::new();
    let gen2 = clock.restart();
    assert_eq!(gen2, 2);
    assert_eq!(FIRED.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Fresh lineage re-handshakes; zombie of gen1 cut.
    let node = SnubberNode::builder("super/hang").build();
    node.enter_with_generation(gen2, 1, 1, || {})
        .expect("gen2 live")
        .commit();
    match node.enter_with_generation(1, 9999, 2, || {}) {
        Err(EnterError::StaleEpoch) => {}
        Err(_) => panic!("wrong refusal"),
        Ok(_) => panic!("zombie must be cut"),
    }
    writer_h.join().expect("frozen worker thaws");
}

/// Supervised erased entry behaves like the canonical worker entry on a
/// single-flight node: one run commits, the slot frees, the next run is
/// admitted. Without a supervisor in the env (`SNUBBER_SUPERVISED` unset)
/// the token emit/commit hooks are silent no-ops — this test pins the
/// slot lifecycle, not the pipe.
#[test]
fn supervised_erased_entry_recycles_single_flight_slot() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let node = SnubberNode::builder("super/erased-slot")
        .max_in_flight(1)
        .trip_after_failures(100)
        .build();
    let rolled = Arc::new(AtomicU32::new(0));

    // Run 1: arm supervised, commit clean — no rollback, slot freed.
    let token = ProcessToken::tagged(13, 1);
    node.enter_supervised_erased(token, {
        let c = Arc::clone(&rolled);
        move || {
            c.fetch_add(1, Ordering::SeqCst);
        }
    })
    .expect("first supervised run admitted")
    .commit();
    assert_eq!(rolled.load(Ordering::SeqCst), 0, "commit disarms");

    // Run 2, immediate restart: must be admitted, not BulkheadFull.
    let token2 = ProcessToken::tagged(13, 2);
    node.enter_supervised_erased(token2, {
        let c = Arc::clone(&rolled);
        move || {
            c.fetch_add(1, Ordering::SeqCst);
        }
    })
    .expect("immediate supervised restart admitted")
    .commit();

    let s = node.stats();
    assert_eq!(s.total_started, 2, "one probe admission per run");
    assert_eq!(s.committed, 2, "both probes committed clean");
    assert_eq!(s.bulkhead_rejections, 0, "no slot ever stuck");
    assert_eq!(
        rolled.load(Ordering::SeqCst),
        0,
        "no rollback on clean path"
    );
}
