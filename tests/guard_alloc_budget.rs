//! Zero-leak gate: a counting `#[global_allocator]` proves that a settled
//! `TrackedGuard` returns every allocated byte.
//!
//! Regression for the `mem::forget(self)` settle-path leak: one
//! `Arc<Mutex<NodeInner>>` (~2.4 KiB) was stranded per transaction, which
//! stayed invisible in short tests and surfaced as +24 MB over 10k OS kills.
//! Counters are thread-local (see below): a process-wide counter reported
//! 900 B of libtest-main-thread allocations as a node leak on CI.
//! Allocation counting cannot run under Miri, same as `checkpoint_budget`.

use snubber::SnubberNode;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

// Per-thread, not global: libtest runs every test on its own worker thread
// and the harness main thread may allocate once (900 B observed on CI)
// while the burst runs. A process-wide counter billed that noise to the
// node and failed the gate without any node allocation leaking. The crate
// itself uses no thread locals, so every byte the node allocates is still
// counted on the thread that settles the guard.
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

#[test]
fn settlement_is_byte_neutral_across_100k_transactions() {
    // Baseline BEFORE any node exists: one leaked `Arc<Mutex<NodeInner>>`
    // clone per settle keeps the whole NodeInner allocation alive past the
    // node's drop, so measuring inside the node's lifetime would see nothing.
    let baseline = net();
    run_storm("alloc/leak-gate/1");
    let first = net() - baseline;
    // Second, identical storm on a fresh node. Strict: platform lazies and
    // first-use caches are already warm, so any growth here is a real leak.
    run_storm("alloc/leak-gate/2");
    let second = net() - baseline - first;
    assert_eq!(
        second, 0,
        "a repeated 100_000-transaction storm changed this thread's heap by \
         {second} B after the node was dropped (first storm: {first} B); a \
         settled guard must reclaim its Arc, closure and profile"
    );
    // The first storm may pay one-time lazy allocations, but a per-transaction
    // leak is megabytes (100_000 x ~2.4 KiB) and a per-node leak fails above.
    assert!(
        first < 4096,
        "the first storm grew this thread's heap by {first} B — that is a \
         per-node leak, not one-time startup noise"
    );
}

fn run_storm(name: &'static str) {
    let node = SnubberNode::builder(name).build();
    burst(&node, 1_000); // warm lazy internals; not measured separately
    burst(&node, 100_000);
}
