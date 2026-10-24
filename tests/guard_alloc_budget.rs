//! Zero-leak gate: a counting `#[global_allocator]` proves that a settled
//! guard returns every allocated byte.
//!
//! Regression for the `mem::forget(self)` settle-path leak: one
//! `Arc<Mutex<NodeInner>>` (~2.4 KiB) was stranded per transaction, which
//! stayed invisible in short tests and surfaced as +24 MB over 10k OS kills.
//! Both layers are covered: `TrackedGuard` storms and the core guards whose
//! closures capture a heap payload.
//!
//! The storm runs on a thread this test owns, and the counter is thread-local,
//! so the measurement sees ONLY bytes the guards allocate on the thread that
//! settles them. A process-wide counter billed libtest's own setup to the
//! node (900 B on Ubuntu CI); counting on the libtest worker still shares
//! that thread with the harness. Allocation counting cannot run under Miri,
//! same as `checkpoint_budget`.

use snubber::{ErasedGuard, SnubberGuard, SnubberNode};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static NET_BYTES: Cell<isize> = const { Cell::new(0) };
}

fn add_bytes(delta: isize) {
    // `try_with`: allocations during TLS teardown must not re-initialize
    // (or panic in) the counter; their bytes are irrelevant to this test.
    let _ = NET_BYTES.try_with(|cell| cell.set(cell.get() + delta));
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            add_bytes(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        add_bytes(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = System.realloc(ptr, layout, new_size);
        if !out.is_null() {
            add_bytes(new_size as isize - layout.size() as isize);
        }
        out
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn net() -> isize {
    NET_BYTES.with(Cell::get)
}

fn burst(node: &SnubberNode, cycles: usize) {
    for _ in 0..cycles {
        let guard = node.enter_auto(|| {}).expect("admit");
        let _ = guard.commit();
    }
}

fn storm(name: &'static str) {
    let node = SnubberNode::builder(name).build();
    burst(&node, 1_000); // warm lazy internals; not measured separately
    burst(&node, 100_000);
}

#[test]
fn settlement_is_byte_neutral_across_100k_transactions() {
    let gate = std::thread::spawn(|| {
        // Baseline before any node exists on this thread. One leaked
        // `Arc<Mutex<NodeInner>>` clone per settle keeps the whole NodeInner
        // allocation alive past the node's drop, so measuring inside the
        // node's lifetime would see nothing.
        let baseline = net();
        storm("alloc/leak-gate/1");
        let first = net() - baseline;
        // Second, identical storm on a fresh node. Strict: this thread's
        // first-use caches are already warm, so any growth is a real leak.
        storm("alloc/leak-gate/2");
        let second = net() - baseline - first;
        (first, second)
    });
    let (first, second) = gate.join().expect("alloc-gate thread panicked");

    assert_eq!(
        second, 0,
        "a repeated 100_000-transaction storm changed the heap by {second} B \
         after the node was dropped (first storm: {first} B); a settled guard \
         must reclaim its Arc, closure and profile"
    );
    // The first storm may pay one-time lazy allocations, but a per-transaction
    // leak is megabytes (100_000 x ~2.4 KiB) and a per-node leak fails above.
    assert!(
        first < 4096,
        "the first storm grew the heap by {first} B — that is a per-node leak, \
         not one-time startup noise"
    );
}

/// Layer-1 settle paths. Each guard owns a closure that captured a heap
/// payload; commit must drop the closure with the guard, abort must run it,
/// and neither may forget the guard itself. 200k guards per phase.
fn layer1_storm() {
    for _ in 0..50_000 {
        let payload = Box::new(0u64);
        let guard = SnubberGuard::new(1, 1, move || drop(payload));
        let _ = guard.commit();
    }
    for _ in 0..50_000 {
        let payload = Box::new(0u64);
        let guard = SnubberGuard::new(1, 1, move || drop(payload));
        let _ = guard.abort("alloc gate");
    }
    for _ in 0..50_000 {
        let payload = Box::new(0u64);
        ErasedGuard::new(move || drop(payload)).commit();
    }
    for _ in 0..50_000 {
        let payload = Box::new(0u64);
        ErasedGuard::new(move || drop(payload)).abort("alloc gate");
    }
}

#[test]
fn layer1_settlement_is_byte_neutral_across_200k_guards() {
    let gate = std::thread::spawn(|| {
        let baseline = net();
        layer1_storm();
        let first = net() - baseline;
        layer1_storm();
        let second = net() - baseline - first;
        (first, second)
    });
    let (first, second) = gate.join().expect("layer-1 alloc-gate thread panicked");

    assert_eq!(
        second, 0,
        "a repeated 200_000-guard settlement storm changed the heap by {second} B \
         (first storm: {first} B); commit and abort must drop the closure and the \
         guard itself — `mem::forget` on a settle path leaks both"
    );
    assert!(
        first < 4096,
        "the first storm grew the heap by {first} B — that is a per-guard leak"
    );
}
