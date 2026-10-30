//! Process lifecycle supervision and token recovery: generation bump drops stale packets,
//! pipe EOF fires the last-armed token. Deterministic supervisor, honest counters.

use snubber::{
    DeathCause, EnterError, GenerationClock, PipeDrain, ProcessToken, SnubberEvent, SnubberNode,
    TransactionHeader, OP_COMMIT,
};

#[test]
fn supervisor_restart_rejects_stale_generation() {
    // gen0 lives to epoch 1420, then dies. gen1 re-handshakes at epoch 1;
    // stale packets from the superseded generation are rejected even with HIGHER epoch.
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
    // Stale packet from the superseded generation — higher epoch, old generation: rejected.
    match node.enter_with_generation(g0, 9999, 3, || {}) {
        Err(EnterError::StaleEpoch) => {}
        Err(_) => panic!("wrong refusal kind"),
        Ok(_) => panic!("stale packet must be rejected"),
    }
    let s = node.stats();
    assert_eq!(s.total_started, 2, "only live generations armed");
    assert_eq!(s.stale_epochs_rejected, 1, "stale packet counted");
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
    // Proves the backpressure fix: reader drains while writer hammers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    let addr = listener.local_addr().expect("addr");
    let (tx_done, rx_done) = std::sync::mpsc::channel::<()>();
    let writer_h = std::thread::spawn(move || {
        let mut w = TcpStream::connect(addr).expect("connect");
        // Arm token (opcode 7) then 2000 commits — drain must keep up,
        // kernel buffer must never block the writer.
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
    // Last-armed token survived: the 7777/9 live one. The 0..2000 commits
    // name no arm, so they settled nothing; the re-arm replaced 4242.
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
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // Channel-backed reader: the sender controls exactly when bytes arrive and
    // when silence begins.  No TCP socket, no SO_RCVTIMEO, no platform quirks.
    // `recv()` blocks until bytes are sent or the sender is dropped (EOF).
    struct ChannelReader {
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
        pending: std::io::Cursor<Vec<u8>>,
    }
    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            // Drain leftover bytes from the previous chunk first.
            if self.pending.position() < self.pending.get_ref().len() as u64 {
                return self.pending.read(buf);
            }
            // Wait for the next chunk from the sender (blocks until data or EOF).
            match self.rx.recv() {
                Ok(chunk) => {
                    self.pending = std::io::Cursor::new(chunk);
                    self.pending.read(buf)
                }
                // Sender dropped: EOF so the drain loop exits cleanly.
                Err(_) => Ok(0),
            }
        }
    }

    let live = ProcessToken::new(31337, 11, [0xBB; 28]);
    let hb = ProcessToken::new(0, OP_HEARTBEAT, [0; 28]);
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(&live.to_bytes());
    payload.extend_from_slice(&hb.to_bytes());

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    // Wrap the sender in an Arc+Mutex so we can keep the channel open (frozen
    // worker: pipe alive, no traffic) while waiting for idle_ms > 0.
    let sender_hold: Arc<Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(Some(tx)));

    let reader = ChannelReader {
        rx,
        pending: std::io::Cursor::new(Vec::new()),
    };

    // Warm up the monotonic clock (static OnceLock inside now_ms) so that when
    // the drain thread calls now_ms() the boot instant is already initialised
    // and elapsed() returns > 0 ms.  Without this the very first now_ms() call
    // returns 0, which idle_ms() treats as "never saw traffic".
    let _warmup = SnubberNode::builder("_warmup").build();
    std::thread::sleep(Duration::from_millis(2));

    let drain = PipeDrain::spawn(reader);

    // Send token bytes to the drain thread.  The drain thread blocks in
    // ChannelReader::read → recv() until this send arrives.
    sender_hold
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .send(payload)
        .expect("channel send");

    // Wait for drain to process the two tokens (last_seen_ms becomes non-zero).
    let mut spins = 0u32;
    while drain.idle_ms() == 0 && spins < 5_000 {
        std::thread::sleep(Duration::from_millis(1));
        spins += 1;
    }
    assert!(
        spins < 5_000,
        "drain received no tokens within 5 s (spins={})",
        spins
    );
    assert!(drain.idle_ms() < 5_000, "traffic seen, clock running");
    // Sender still held → channel open → eof() stays false (frozen worker).
    assert!(!drain.eof(), "frozen worker keeps the reader open");
    assert!(!drain.expired(50_000), "fresh traffic is not expired");

    // Lease 50 ms: sleep past it while the worker is frozen.
    std::thread::sleep(Duration::from_millis(120));
    assert!(
        drain.expired(50),
        "silence past lease = HangTimeout, idle={}",
        drain.idle_ms()
    );

    // Supervisor path: take token → rollback → generation bump.
    static FIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let armed = drain.take_armed().expect("frozen mid-flight token");
    assert_eq!(armed.task, 31337);
    assert_eq!(armed.opcode, 11);
    FIRED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut clock = GenerationClock::new();
    let gen2 = clock.restart();
    assert_eq!(gen2, 2);
    assert_eq!(FIRED.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Fresh lineage re-handshakes; stale packet of gen1 rejected.
    let node = SnubberNode::builder("super/hang").build();
    node.enter_with_generation(gen2, 1, 1, || {})
        .expect("gen2 live")
        .commit();
    match node.enter_with_generation(1, 9999, 2, || {}) {
        Err(EnterError::StaleEpoch) => {}
        Err(_) => panic!("wrong refusal"),
        Ok(_) => panic!("stale generation must be rejected"),
    }

    // Drop the sender so the drain thread exits cleanly (EOF via recv() error).
    *sender_hold.lock().unwrap() = None;
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

#[test]
fn process_died_is_own_kind_not_channel() {
    use snubber::{FaultAction, FaultKind, LogLevel};
    assert_eq!(FaultKind::ProcessDied.index(), 10);
    assert!(matches!(
        snubber::DEFAULT_POLICIES[FaultKind::ProcessDied.index()].action,
        FaultAction::RollbackAndAlert
    ));
    assert_eq!(
        snubber::DEFAULT_POLICIES[FaultKind::ProcessDied.index()].log_level,
        Some(LogLevel::Critical)
    );
    assert_eq!(
        snubber::DEFAULT_POLICIES[FaultKind::ProcessDied.index()].metric_tag,
        Some("process_deaths_total")
    );
    let node = SnubberNode::builder("death/honest").build();
    node.report(FaultKind::ProcessDied, 1, 1);
    let s = node.stats();
    assert_eq!(s.reported[FaultKind::ProcessDied.index()], 1);
    assert_eq!(s.reported[FaultKind::ChannelDisconnected.index()], 0);
    assert_eq!(s.last_fault.map(|f| f.kind), Some(FaultKind::ProcessDied));
    assert!(node.take_alert(), "death must raise an alert latch");
}

#[test]
fn spawn_guarded_binds_thread_guard_leash() {
    use snubber::{Directive, TaskConduit};
    let node = SnubberNode::builder("debt/spawn")
        .max_in_flight(1)
        .trip_after_failures(0)
        .build();
    let (published_tx, published_rx) = std::sync::mpsc::channel();
    let (join, handle) = node.spawn_guarded(
        || {},
        0u64,
        move |conduit: TaskConduit<u64, ()>| {
            conduit.update_state(|s| *s = 1);
            // Signal AFTER the publish. A panicking worker drops the
            // sender, so the wait below fails with `Err` instead of racing
            // a fixed sleep (Miri needs far longer than 20 ms to reach
            // this point, and its clock/sleep do not agree).
            let _ = published_tx.send(());
            loop {
                match conduit.check_directive() {
                    Directive::Cancel(_) | Directive::Preempted(_) => break,
                    Directive::Pause => {
                        let _ = conduit.wait_for_resume();
                    }
                    _ => std::thread::yield_now(),
                }
            }
        },
    );
    // Blocking recv, not a timed poll: Miri schedules the worker while the
    // main thread blocks; a dead worker closes the channel and surfaces
    // here as `Err` rather than hanging on a sleep budget.
    published_rx.recv().expect("worker published via conduit");
    assert_eq!(handle.inspect(), 1, "state visible through the handle");
    handle.send_directive(Directive::Cancel("stop"));
    let res = join.join().expect("worker joined");
    assert!(res.is_ok(), "clean commit, got {res:?}");
    let s = node.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(s.poison_recoveries, 0);
}

#[test]
fn spawn_guarded_refusal_surfaces_in_join() {
    use snubber::{SnubberError, TaskConduit};
    let node = SnubberNode::builder("debt/refuse")
        .max_in_flight(1)
        .trip_after_failures(0)
        .build();
    let _held = node.enter(1, 1, || {}).expect("hold");
    let (join, _handle) = node.spawn_guarded(
        || {},
        0u64,
        |_: TaskConduit<u64, ()>| panic!("work must never run on refusal"),
    );
    let res = join.join().expect("joined");
    match res {
        Err(SnubberError::Admission(_)) => {}
        other => panic!("honest admission boundary refusal, got {other:?}"),
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri does not support posix_spawn / OS process spawning"
)]
fn stdin_digest_chain_is_closed_tick0() {
    use snubber::{StateDigest, STATE_LEN};
    let mut bytes = [0u8; STATE_LEN];
    bytes[0] = 42;
    let exe = std::env::current_exe().expect("exe");
    let successor = exe
        .parent()
        .expect("deps dir")
        .parent()
        .expect("debug dir")
        .join("examples")
        .join("07_state_handoff");
    let successor = if cfg!(windows) {
        successor.with_extension("exe")
    } else {
        successor
    };
    if !successor.exists() {
        // Build examples if not built yet, matching the active profile (debug vs release)
        let mut build_cmd = std::process::Command::new("cargo");
        build_cmd.args(["build", "--example", "07_state_handoff"]);
        if !cfg!(debug_assertions) {
            build_cmd.arg("--release");
        }
        let status = build_cmd.status().expect("build example");
        assert!(status.success());
    }
    assert!(successor.exists(), "build examples first: {successor:?}");
    let digest = StateDigest::new(bytes);
    let mut cmd = std::process::Command::new(&successor);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    use std::io::Write;
    let mut child = cmd.spawn().expect("spawn replacement process");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&digest.bytes).expect("64 B, no block");
    }
    let status = child.wait().expect("reap replacement process");
    assert_eq!(status.code(), Some(42), "mark 42 survived real spawn");

    let mut sink = std::process::Command::new(&successor);
    sink.stdout(std::process::Stdio::null());
    sink.stderr(std::process::Stdio::null());
    let mut child2 = snubber::Supervisor::spawn_with_digest(sink, &digest).expect("spawn via fn");
    let st = child2.wait().expect("reap");
    assert_eq!(st.code(), Some(42), "fn path delivers too");
}

#[test]
fn supervisor_with_lease_terminates_hung_worker_and_rescues_token() {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct ChannelReader {
        rx: std::sync::mpsc::Receiver<Vec<u8>>,
        pending: std::io::Cursor<Vec<u8>>,
    }
    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.position() < self.pending.get_ref().len() as u64 {
                return self.pending.read(buf);
            }
            match self.rx.recv() {
                Ok(chunk) => {
                    self.pending = std::io::Cursor::new(chunk);
                    self.pending.read(buf)
                }
                Err(_) => Ok(0),
            }
        }
    }

    // 1. Worker arms a token and sends it across the pipe
    let token = ProcessToken::new(999, 13, [0xCC; 28]);
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();

    let tx_holder = Arc::new(Mutex::new(Some(tx)));
    let tx_for_worker = Arc::clone(&tx_holder);

    let worker_terminated = Arc::new(AtomicBool::new(false));
    let worker_term_signal = Arc::clone(&worker_terminated);

    // Simulate worker process hanging in a deadlocked rollback/I/O:
    // Holds the pipe open (silence, no EOF, no commit) until supervisor terminates it.
    let worker_thread = std::thread::spawn(move || {
        while !worker_term_signal.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Simulated OS termination: drops write handle, emitting EOF on reader
        let _ = tx_for_worker.lock().unwrap().take();
    });

    // Warm up monotonic clock so now_ms() > 0
    let _warmup = SnubberNode::builder("_warmup2").build();
    std::thread::sleep(Duration::from_millis(2));

    let reader = ChannelReader {
        rx,
        pending: std::io::Cursor::new(Vec::new()),
    };

    let drain = PipeDrain::spawn(reader);

    // Send armed token + heartbeat to update last_seen_ms
    let hb = ProcessToken::new(0, snubber::OP_HEARTBEAT, [0; 28]);
    let mut payload = token.to_bytes().to_vec();
    payload.extend_from_slice(&hb.to_bytes());

    tx_holder
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .send(payload)
        .expect("send token");

    // Wait until drain processes the armed token
    let mut spins = 0u32;
    while drain.idle_ms() == 0 && spins < 5_000 {
        std::thread::sleep(Duration::from_millis(1));
        spins += 1;
    }
    assert!(spins < 5_000, "token received");

    // Silence lease: 40 ms
    let lease_ms = 40u64;
    let mut killed = false;

    // Supervisor watchdog loop (same logic as run_supervised_guard_with_lease)
    for _ in 0..100 {
        if drain.expired(lease_ms) {
            // Hang timeout triggered! Supervisor terminates hung worker from outside.
            worker_terminated.store(true, Ordering::SeqCst);
            killed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(
        killed,
        "supervisor must trigger termination on hang past lease_ms"
    );
    worker_thread.join().expect("worker terminated");

    // Wait for EOF on pipe after termination
    for _ in 0..50 {
        if drain.eof() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(drain.eof(), "pipe must hit EOF after worker is killed");

    // Reclaim armed token
    let rescued = Arc::new(AtomicU32::new(0));
    if let Some(t) = drain.take_armed() {
        assert_eq!(t.task, 999);
        assert_eq!(t.opcode, 13);
        rescued.store(1, Ordering::SeqCst);
    }

    assert_eq!(
        rescued.load(Ordering::SeqCst),
        1,
        "token rescued from hung worker"
    );
}

#[test]
fn drain_counters_split_arms_commits_and_survive_noise() {
    let mut wire = Vec::new();
    wire.extend_from_slice(&ProcessToken::new(11, 7, [1u8; 28]).to_bytes());
    wire.extend_from_slice(b"log noise between frames\n");
    wire.extend_from_slice(&ProcessToken::commit(11).to_bytes());
    wire.extend_from_slice(&ProcessToken::new(12, 7, [2u8; 28]).to_bytes());

    let drain = PipeDrain::drain_sync(std::io::Cursor::new(wire));
    let counters = drain.counters();
    assert_eq!(counters.frames, 3);
    assert_eq!(counters.arms, 2);
    assert_eq!(counters.commits, 1);

    let armed = drain.take_armed().expect("last arm stays armed");
    assert_eq!(armed.task, 12);
    assert_eq!(drain.counters(), counters, "counters are stable snapshots");
}
