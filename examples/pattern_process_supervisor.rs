//! Pattern 04: Process Supervisor & Pipe Drain
//!
//! Demonstrates Layer 4 multi-process supervision:
//! Running an untrusted or crash-prone child process (e.g. WebView, plugin, GPU worker)
//! across a framed IPC boundary. If the child process is abruptly killed by the OS
//! (SIGKILL, TerminateProcess), the supervisor detects EOF without deadlocking and
//! releases the external resource on the parent side.

use snubber::{PipeDrain, ProcessToken};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    println!("=== Pattern 04: Multi-Process Isolation & Token Rescue ===");

    let hardware_token_held = Arc::new(AtomicBool::new(true));

    // We simulate an anonymous IPC pipe between processes using loopback stream
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let hw = Arc::clone(&hardware_token_held);
    let child_worker = thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("connect");

        // 1. Child arms token 4242 with opcode 7 (custom protected operation)
        let mut payload = [0u8; 28];
        payload[0] = 0xAA;
        let armed = ProcessToken::new(4242, 7, payload);
        stream.write_all(&armed.to_bytes()).expect("write armed");

        // 2. Simulate abrupt process death (crash / TerminateProcess / SIGKILL)
        // Dropping stream closes the write-end, generating EOF on supervisor reader.
        drop(stream);
    });

    let (reader, _) = listener.accept().expect("accept");
    let drain = PipeDrain::spawn(reader);

    child_worker.join().unwrap();

    // Wait for EOF detection by supervisor
    let mut spins = 0u32;
    while !drain.eof() && spins < 1000 {
        thread::sleep(Duration::from_millis(1));
        spins += 1;
    }

    assert!(drain.eof(), "Supervisor must detect EOF on process kill");

    // Supervisor inspects armed token left by dead process and triggers safety release
    if let Some(token) = drain.take_armed() {
        println!(
            "[Supervisor] Child process died abruptly! Rescuing armed task {}...",
            token.task
        );
        hw.store(false, Ordering::SeqCst);
        println!("[Supervisor] Hardware interlock safely disarmed.");
    }

    assert!(
        !hardware_token_held.load(Ordering::SeqCst),
        "Hardware token must be released"
    );
    println!("\nVerification: Supervisor cleanly isolated child termination without deadlocking.");
}
