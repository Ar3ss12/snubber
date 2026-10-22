//! Heap budget proof for checkpointing: a counting `#[global_allocator]`
//! measures the peak delta of the streaming writer and of the successor's
//! verifier. The streaming path must stay within one 64 KiB buffer; the
//! naive `to_vec()` path must allocate the whole state.
//!
//! Allocation counting cannot run under Miri (custom global allocators are
//! unsupported), so this binary is excluded from the Miri gate.

#[path = "../examples/_shared/checkpoint.rs"]
mod checkpoint;

use checkpoint::{crc32, make_state, seal, verify, write_checkpoint, FILE_ID};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static MEASURE: Mutex<()> = Mutex::new(());

struct Counting;

fn note_peak(live: usize) {
    let mut peak = PEAK.load(Ordering::Relaxed);
    while live > peak {
        match PEAK.compare_exchange_weak(peak, live, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(now) => peak = now,
        }
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            note_peak(live);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = System.realloc(ptr, layout, new_size);
        if !out.is_null() {
            if new_size >= layout.size() {
                let grow = new_size - layout.size();
                let live = LIVE.fetch_add(grow, Ordering::Relaxed) + grow;
                note_peak(live);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn measure<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let _guard = MEASURE.lock().expect("measure lock");
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst).saturating_sub(baseline))
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
