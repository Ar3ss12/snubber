//! Pattern 17: Impedance Bridge — 10 000 msg/s producer, 60 FPS consumer.
//!
//! Speed mismatch has exactly three honest answers; this demo shows two of
//! them side by side on the same producer:
//!
//! * **Raw stream** (`mpsc::channel`) — the consumer takes one event per
//!   frame. The queue grows without bound; at the end almost every event is
//!   unread work for a screen that has already moved on.
//! * **Conflation** ([`ConflateSender`]) — one slot, latest wins. The
//!   consumer takes the newest snapshot per frame; the rest is counted as
//!   `coalesced`, not queued.
//! * **Pacing** ([`gateway`]) — bounded queue with a 75% / 50% high-low
//!   watermark latch. The producer is told to wait (`retry_after_ms`)
//!   instead of piling up work.
//!
//! Run with:
//! ```powershell
//! cargo run --example 17_impedance_bridge
//! cargo run --example 17_impedance_bridge -- --ms 3000 --fps 144 --rate 20000
//! ```

use snubber::{gateway, ConflateSender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Snapshot {
    seq: u64,
    checksum: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ms = arg_u64(&args, "--ms", 1500);
    let fps = arg_u64(&args, "--fps", 60).max(1);
    let rate = arg_u64(&args, "--rate", 10_000).max(1);
    let frame = Duration::from_micros(1_000_000 / fps);

    println!("=== Pattern 17: Impedance Bridge ===");
    println!("[PRODUCER] rate_hz={rate} for_ms={ms} consumer_fps={fps}");

    let (conflate_tx, conflate_rx) = ConflateSender::<Snapshot>::pair();
    let (raw_tx, raw_rx) = channel::<Snapshot>();
    let done = Arc::new(AtomicBool::new(false));

    let producer = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let started = Instant::now();
            let deadline = started + Duration::from_millis(ms);
            let per_tick = (rate / 1000).max(1);
            let mut seq = 0u64;
            while Instant::now() < deadline {
                for _ in 0..per_tick {
                    let snap = Snapshot {
                        seq,
                        checksum: seq.wrapping_mul(2_654_435_761),
                    };
                    seq += 1;
                    let _ = conflate_tx.publish(snap);
                    let _ = raw_tx.send(snap);
                }
                thread::sleep(Duration::from_millis(1));
            }
            done.store(true, Ordering::Release);
            (seq, started.elapsed())
        })
    };

    // The screen: one raw event per frame (the stream cannot be faster than
    // the display), one conflated snapshot per frame (only the latest matters).
    let mut frames = 0u64;
    let mut raw_delivered = 0u64;
    let mut conflated_delivered = 0u64;
    let mut newest = None;
    while !done.load(Ordering::Acquire) {
        thread::sleep(frame);
        frames += 1;
        if raw_rx.try_recv().is_ok() {
            raw_delivered += 1;
        }
        if let Some(snap) = conflate_rx.take_latest() {
            newest = Some(snap);
            conflated_delivered += 1;
        }
    }
    let (events, elapsed) = producer.join().expect("producer thread");
    if let Some(snap) = conflate_rx.take_latest() {
        newest = Some(snap);
        conflated_delivered += 1;
    }
    let raw_backlog: u64 = raw_rx.try_iter().count() as u64;
    let elapsed_ms = elapsed.as_millis().max(1) as u64;
    let observed_hz = events * 1000 / elapsed_ms;

    println!(
        "[RAW] events={events} observed_hz={observed_hz} frames={frames} delivered={raw_delivered} backlog={raw_backlog}"
    );
    println!(
        "[CONFLATE] delivered={conflated_delivered} coalesced={} newest_seq={} newest_checksum={}",
        conflate_rx.conflated(),
        newest.map_or(0, |s| s.seq),
        newest.map_or(0, |s| s.checksum)
    );

    gateway_pacing();

    println!(
        "[VERDICT] raw_stream_retained={raw_backlog} events; conflation_retained=1 slot and counted {} replacements",
        conflate_rx.conflated()
    );
}

/// Bounded queue + HWM/LWM latch: the producer is paced, not ignored.
fn gateway_pacing() {
    const CAP: usize = 16;
    let (tx, host) = gateway::<u32>(CAP);

    let mut sent = 0u64;
    for i in 0..12 {
        if tx.send(i).is_ok() {
            sent += 1;
        }
    }
    // Depth 12 == 75% of 16: the latch is armed.
    let first = tx.send(99).expect_err("HWM must refuse");
    let depth = tx.depth();
    let retry_before = first.retry_after_ms();

    // Consumer reports 16 ms per item: the advised wait follows the sample.
    host.note_service_ms(16);
    let second = tx.send(99).expect_err("latch holds until LWM");
    let retry_after_sample = second.retry_after_ms();

    // Drain below 50% (depth <= 8) and the latch releases.
    while host.try_recv().is_some() {}
    let released = tx.send(100).is_ok();
    if released {
        sent += 1;
    }
    let stats = tx.stats();

    println!(
        "[GATEWAY] capacity={CAP} sent={sent} admitted={} throttled={} shed={} depth_at_hwm={depth} retry_after_ms={retry_before} retry_after_sample_ms={retry_after_sample} released_after_drain={released}",
        stats.admitted, stats.throttled, stats.shed
    );
}

fn arg_u64(args: &[String], key: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
