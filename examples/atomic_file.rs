//! Atomic file/config update: tmp + lock must vanish on ANY failure,
//! or the daemon wedges on next boot ("resource locked").
//!
//! Run: `cargo run --example atomic_file`

use snubber::SnubberNode;
use std::sync::atomic::{AtomicBool, Ordering};

static TMP: AtomicBool = AtomicBool::new(false);
static LOCK: AtomicBool = AtomicBool::new(false);

fn main() {
    let node = SnubberNode::desktop("sys/config");
    TMP.store(true, Ordering::SeqCst);
    LOCK.store(true, Ordering::SeqCst);
    let guard = node
        .enter(1, 1, || {
            TMP.store(false, Ordering::SeqCst);
            LOCK.store(false, Ordering::SeqCst);
        })
        .expect("admit");
    // Disk error mid-write → early return WITHOUT commit.
    drop(guard);
    assert!(!TMP.load(Ordering::SeqCst), "tmp removed");
    assert!(!LOCK.load(Ordering::SeqCst), "lock released");
    assert_eq!(node.stats().implicit_drops, 1);
    println!("ATOMIC_FILE: disk clean, daemon will boot");
}
