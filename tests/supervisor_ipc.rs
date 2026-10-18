//! Debts closed: spawn leash + stdin digest chain.
use snubber::{Directive, SnubberError, SnubberNode, TaskConduit};
#[test]
fn spawn_guarded_binds_thread_guard_leash() {
    let node = SnubberNode::builder("debt/spawn")
        .max_in_flight(1)
        .trip_after_failures(0)
        .build();
    let (join, handle) = node.spawn_guarded(
        || {},
        0u64,
        |conduit: TaskConduit<u64, ()>| {
            conduit.update_state(|s| *s = 1);
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
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(handle.inspect(), 1, "worker published via conduit");
    handle.send_directive(Directive::Cancel("stop"));
    let res = join.join().expect("worker joined");
    assert!(res.is_ok(), "clean commit, got {res:?}");
    let s = node.stats();
    assert_eq!(s.total_started, 1);
    assert_eq!(s.committed, 1);
    assert_eq!(s.poison_recoveries, 0);
    println!("\n[SPAWN]: started=1 committed=1 poison=0");
}
#[test]
fn spawn_guarded_refusal_surfaces_in_join() {
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
    println!("\n[SPAWN]: refusal=admission work=never poison=0");
}
#[test]
fn stdin_digest_chain_is_closed_tick0() {
    use snubber::{StateDigest, STATE_LEN};
    // Replacement binary: examples/process_state_handoff.rs reads stdin tick 0 via
    // take_stdin_digest() and exits with bytes[0] as its code.
    // Parent half: build the example, spawn it with piped stdin,
    // write 64 B + EOF exactly like spawn_with_digest does.
    let mut bytes = [0u8; STATE_LEN];
    bytes[0] = 42;
    let exe = std::env::current_exe().expect("exe");
    let successor = exe
        .parent()
        .expect("deps dir")
        .parent()
        .expect("debug dir")
        .join("examples")
        .join("process_state_handoff");
    let successor = if cfg!(windows) {
        successor.with_extension("exe")
    } else {
        successor
    };
    assert!(successor.exists(), "build examples first: {successor:?}");
    let digest = StateDigest::new(bytes);
    let mut cmd = std::process::Command::new(&successor);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    // Same body as Supervisor::spawn_with_digest (kept inline: the
    // test must prove the CONTRACT, not call the fn blindly — the
    // fn itself is covered by compilation + the sink half below).
    use std::io::Write;
    let mut child = cmd.spawn().expect("spawn replacement process");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&digest.bytes).expect("64 B, no block");
    }
    let status = child.wait().expect("reap replacement process");
    assert_eq!(status.code(), Some(42), "mark 42 survived real spawn");
    // Parent half via the REAL fn: sink child exits fast, no stall.
    let mut sink = std::process::Command::new(&successor);
    sink.stdout(std::process::Stdio::null());
    sink.stderr(std::process::Stdio::null());
    let mut child2 = snubber::Supervisor::spawn_with_digest(sink, &digest).expect("spawn via fn");
    let st = child2.wait().expect("reap");
    assert_eq!(st.code(), Some(42), "fn path delivers too");
    println!("\n[DIGEST]: spawned=2 mark=42 successor=42 stdin=closed");
}
