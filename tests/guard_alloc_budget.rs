//! Zero-leak gate: a counting `#[global_allocator]` proves that a settled
//! `TrackedGuard` returns every allocated byte.
//!
//! Regression for the `mem::forget(self)` settle-path leak: one
//! `Arc<Mutex<NodeInner>>` (~2.4 KiB) was stranded per transaction, which
//! stayed invisible in short tests and surfaced as +24 MB over 10k OS kills.
//! Allocation counting cannot run under Miri, same as `checkpoint_budget`.

use snubber::SnubberNode;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

static NET_BYTES: AtomicIsize = AtomicIsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            NET_BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        NET_BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = System.realloc(ptr, layout, new_size);
        if !out.is_null() {
            NET_BYTES.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        out
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn net() -> isize {
    NET_BYTES.load(Ordering::SeqCst)
}

fn burst(node: &SnubberNode, cycles: usize) {
    for _ in 0..cycles {
        let guard = node.enter_auto(|| {}).expect("admit");
        let _ = guard.commit();
    }
}

#[test]
fn settlement_is_byte_neutral_across_100k_transactions() {
    // Baseline BEFORE the node exists: one leaked `Arc<Mutex<NodeInner>>`
    // clone per settle keeps the whole NodeInner allocation alive past the
    // node's drop, so measuring inside the node's lifetime would see nothing.
    let baseline = net();
    {
        let node = SnubberNode::builder("alloc/leak-gate").build();
        burst(&node, 1_000); // warm lazy internals; not measured separately
        burst(&node, 100_000);
    }
    let delta = net() - baseline;
    assert_eq!(
        delta, 0,
        "100_000 settled transactions changed the heap by {delta} B after the \
         node was dropped; a settled guard must reclaim its Arc, closure and profile"
    );
}
