//! Cold snapshot persistence: the APP owns the thread, path and timer.
//!
//! The crate gives `SnapshotV1::encode() -> [u8; 512]`; this example shows
//! the app side: flush every 10 s + on clean shutdown, verify on boot.
//! No fs/thread code lives inside `snubber` — sterility preserved.
//!
//! Run: `cargo run --example persist_snapshot`

use snubber::{AnomalyPolicy, SnapshotV1, SnubberNode};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn snap_path() -> PathBuf {
    std::env::temp_dir().join("snubber_app.snub")
}

fn flush(node: &SnubberNode, generation: u64) {
    let snap = node.snapshot_v1_in(generation);
    let bytes = snap.encode();
    debug_assert_eq!(bytes.len(), snubber::SNAPSHOT_LEN);
    // Crash-safe-ish: write tmp + rename (no torn 512 B frame).
    let path = snap_path();
    let tmp = path.with_extension("snub.tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

fn boot_diagnosis() {
    let path = snap_path();
    let Ok(bytes) = fs::read(&path) else {
        eprintln!("[snub] no previous snapshot — cold boot");
        return;
    };
    if bytes.len() != snubber::SNAPSHOT_LEN {
        eprintln!("[snub] torn snapshot ({} B) — ignoring", bytes.len());
        return;
    }
    let mut fixed = [0u8; snubber::SNAPSHOT_LEN];
    fixed.copy_from_slice(&bytes);
    match SnapshotV1::decode(&fixed) {
        Err(e) => eprintln!("[snub] snapshot corrupt ({e}) — starting fresh"),
        Ok(snap) => {
            let policy = AnomalyPolicy::default();
            let hits = snubber::find_anomalies_for_snapshot("prev_boot", &snap, &policy);
            if hits.is_empty() {
                eprintln!("[snub] previous shutdown clean (gen {})", snap.generation);
            } else {
                for h in &hits {
                    eprintln!("[snub] {}", snubber::diagnostic_line("prev_boot", h, &snap));
                }
            }
        }
    }
}

fn main() {
    boot_diagnosis();

    let node = SnubberNode::builder("click_worker")
        .max_in_flight(4)
        .build();
    // Simulate work.
    for i in 1..=200u64 {
        if let Ok(g) = node.enter(i, i, || {}) {
            g.commit();
        }
    }

    // Flusher thread: every 10 s (shortened to 1 s in the example).
    let flusher = node.clone();
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            thread::sleep(Duration::from_secs(1));
            flush(&flusher, 4);
        }
    });
    let _ = handle.join();
    // Clean shutdown flush.
    flush(&node, 4);
    eprintln!("[snub] flushed {}", snap_path().display());
}
