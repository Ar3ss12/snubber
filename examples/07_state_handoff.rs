//! Pattern 07: State Handoff (Generational Successor Transfer)
//!
//! State handoff replacement process probe: reads stdin on startup, exits with bytes[0].
//! Used by process supervisors to transfer serialized state digests across process boundaries.
//!
//! Run with:
//! ```powershell
//! cargo run --example 07_state_handoff
//! ```

fn main() {
    let mark = snubber::take_stdin_digest().map_or(99, |d| d.bytes[0]);
    std::process::exit(mark as i32);
}
