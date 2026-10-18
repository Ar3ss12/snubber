//! State handoff replacement process probe: reads stdin on startup, exits with bytes[0].
fn main() {
    let mark = snubber::take_stdin_digest().map_or(99, |d| d.bytes[0]);
    std::process::exit(mark as i32);
}
