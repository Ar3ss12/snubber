//! Heap budget proof for checkpointing: a counting `#[global_allocator]`
//! measures the peak delta of the streaming writer and of the successor's
//! verifier. The streaming path must stay within one 64 KiB buffer; the
//! naive `to_vec()` path must allocate the whole state.
//!
//! The counter is thread-local (same shape as `guard_alloc_budget`): only
//! bytes allocated on the measuring thread are billed, so a parallel test
//! thread or the libtest harness cannot inflate the peak of another
//! measurement — a process-global counter with a mutex guard still shared
//! the window with every other allocation in the process. Allocation
//! counting cannot run under Miri (custom global allocators are
//! unsupported), so this binary is excluded from the Miri gate.

#[path = "../examples/_shared/checkpoint.rs"]
mod checkpoint;

use checkpoint::{crc32, make_state, seal, verify, write_checkpoint, FILE_ID};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;

thread_local! {
    static NET_BYTES: Cell<isize> = const { Cell::new(0) };
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static PEAK_BYTES: Cell<isize> = const { Cell::new(isize::MIN) };
}

fn add_bytes(delta: isize) {
    // `try_with`: allocations during TLS teardown must not re-initialize
    // (or panic in) the counters; their bytes are irrelevant to the gate.
    let _ = NET_BYTES.try_with(|net| {
        let live = net.get() + delta;
        net.set(live);
        let _ = MEASURING.try_with(|measuring| {
            if measuring.get() {
                let _ = PEAK_BYTES.try_with(|peak| {
                    if live > peak.get() {
                        peak.set(live);
                    }
                });
            }
        });
    });
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

fn measure<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let baseline = NET_BYTES.with(|n| n.get());
    PEAK_BYTES.with(|p| p.set(baseline));
    MEASURING.with(|m| m.set(true));
    let out = f();
    MEASURING.with(|m| m.set(false));
    let peak = PEAK_BYTES.with(|p| p.get());
    (out, (peak - baseline).max(0) as usize)
}

fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("snubber_budget_{tag}_{}.sncb", std::process::id()))
}

#[test]
fn streaming_write_stays_within_one_buffer() {
    let state = make_state(8 * 1024 * 1024);
    let path = scratch("stream");

    let (crc, peak) = measure(|| write_checkpoint(&path, &state).expect("streaming write"));
    assert_eq!(crc, crc32(0, &state), "chained CRC == one-shot CRC");
    assert!(
        peak <= 128 * 1024,
        "streaming write grew the heap by {peak} B for an 8 MiB body"
    );

    let (copy_len, naive_peak) = measure(|| {
        let copy = state.to_vec();
        copy.len()
    });
    assert_eq!(copy_len, state.len());
    assert!(
        naive_peak >= state.len(),
        "naive copy grew the heap by {naive_peak} B, expected >= {} B",
        state.len()
    );
    assert!(
        peak * 8 <= naive_peak,
        "streaming {peak} B vs naive {naive_peak} B — no order-of-magnitude win"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn successor_verify_streams_the_body() {
    let state = make_state(4 * 1024 * 1024);
    let path = scratch("verify");
    let crc = write_checkpoint(&path, &state).expect("seal");
    let digest = seal(4, 2, state.len() as u64, crc, &FILE_ID);

    let (total, peak) = measure(|| verify(&digest, &path).expect("verify"));
    assert_eq!(total, state.len() as u64);
    assert!(
        peak <= 128 * 1024,
        "verify grew the heap by {peak} B for a 4 MiB body"
    );

    let _ = std::fs::remove_file(&path);
}
