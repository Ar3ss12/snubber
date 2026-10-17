//! Real-time audio (48 kHz): NaN/overflow must become silence, never a
//! 140 dB blast. Core-only path: static membrane, zero-alloc rollback.
//!
//! Run: `cargo run --example audio_silence`

use snubber::{EpochGate, SnubberGuard, TransactionHeader};

fn main() {
    let mut gate = EpochGate::new();
    let mut buf = [0.5f32; 64];
    // Poisoned block: NaN sneaks into the DSP chain.
    buf[7] = f32::NAN;
    let header = TransactionHeader::new(1, 1);
    assert!(gate.observe(&header), "fresh block admitted");
    // Membrane: finite + in [-1, 1], else the block becomes silence.
    let poisoned = buf.iter().any(|s| !s.is_finite() || s.abs() > 1.0);
    if poisoned {
        buf.fill(0.0); // rollback = digital silence (no blast)
    }
    // Guard owns the same decision for the books: muted => implicit drop
    // path (rollback already applied above), clean => commit.
    let guard = SnubberGuard::new(header.epoch, header.task_id, || {});
    if poisoned {
        drop(guard);
    } else {
        guard.commit();
    }
    assert!(buf.iter().all(|&s| s == 0.0), "block muted, monitors alive");
    println!("AUDIO_SILENCE: NaN block became silence, no blast");
}
