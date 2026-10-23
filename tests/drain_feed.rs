//! Incremental `PipeDrain::feed` parser, transport-agnostic token sink and
//! injectable drain clock.
//!
//! These paths serve embedded/UART bridges and async runtimes that cannot
//! hand the drain an `io::Read`; the OS reader thread keeps using the same
//! scanner underneath.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use snubber::{
    PipeDrain, ProcessToken, StateDigest, OP_COMMIT, OP_HEARTBEAT, OP_SNAPSHOT, OP_STATE,
};

#[test]
fn byte_by_byte_arm_frame_is_one_arm() {
    let drain = PipeDrain::manual();
    let frame = ProcessToken::simple(11, 5, 9).to_bytes();

    let mut delivered = 0usize;
    for byte in frame {
        delivered += drain.feed(&[byte]);
    }

    assert_eq!(delivered, 1, "only the final byte completes the frame");
    let counters = drain.counters();
    assert_eq!(counters.frames, 1);
    assert_eq!(counters.arms, 1);
    assert_eq!(counters.commits, 0);
    assert!(!drain.eof(), "feeding is not EOF");
    assert_eq!(drain.take_armed().map(|token| token.task), Some(11));
}

#[test]
fn two_frames_in_one_chunk_are_both_delivered() {
    let drain = PipeDrain::manual();
    let mut chunk = ProcessToken::simple(7, 9, 1).to_bytes().to_vec();
    chunk.extend_from_slice(&ProcessToken::commit(7).to_bytes());

    assert_eq!(drain.feed(&chunk), 2);
    let counters = drain.counters();
    assert_eq!(counters.frames, 2);
    assert_eq!(counters.arms, 1);
    assert_eq!(counters.commits, 1);
    assert!(
        drain.take_armed().is_none(),
        "the commit settled the armed token"
    );
}

#[test]
fn garbage_around_frames_keeps_alignment() {
    let drain = PipeDrain::manual();
    let first = ProcessToken::simple(1, 5, 1).to_bytes();
    let second = ProcessToken::simple(2, 5, 2).to_bytes();

    let mut chunk = b"worker: boot ok\n".to_vec();
    chunk.extend_from_slice(&first);
    chunk.extend_from_slice(b"tick\n");
    chunk.extend_from_slice(&second);

    assert_eq!(drain.feed(&chunk), 2);
    assert_eq!(drain.counters().frames, 2);
    assert_eq!(drain.take_armed().map(|token| token.task), Some(2));
}

#[test]
fn a_torn_chunk_boundary_inside_a_body_is_reassembled() {
    let drain = PipeDrain::manual();
    let first = ProcessToken::simple(1, 5, 1).to_bytes();
    let second = ProcessToken::simple(2, 5, 2).to_bytes();

    let mut chunk = b"noise".to_vec();
    chunk.extend_from_slice(&first);
    chunk.extend_from_slice(&second[..10]);
    assert_eq!(drain.feed(&chunk), 1, "first frame fits, second is torn");

    assert_eq!(drain.feed(&second[10..]), 1);
    assert_eq!(drain.counters().frames, 2);
    assert_eq!(drain.take_armed().map(|token| token.task), Some(2));
}

#[test]
fn bytes_inserted_inside_a_body_mutate_that_frame_only() {
    // The 40-byte body is opaque: inserted bytes are body bytes, not a
    // scan point. The next `SNUB` magic re-synchronizes the stream.
    let drain = PipeDrain::manual();
    let frame = ProcessToken::simple(42, 5, 1).to_bytes();

    assert_eq!(drain.feed(&frame[..10]), 0);
    assert_eq!(drain.feed(b"x"), 0);
    assert_eq!(drain.feed(&frame[10..]), 1);

    let token = drain.take_armed().expect("the mutated body still parses");
    assert_ne!(token.task, 42, "the body is opaque by contract");

    let next = ProcessToken::simple(43, 5, 1).to_bytes();
    assert_eq!(drain.feed(&next), 1, "the next magic re-synchronizes");
    assert_eq!(drain.take_armed().map(|token| token.task), Some(43));
}

#[test]
fn finish_drops_the_torn_tail_and_seals_the_drain() {
    let drain = PipeDrain::manual();
    let frame = ProcessToken::simple(3, 5, 1).to_bytes();

    assert_eq!(drain.feed(&frame[..43]), 0);
    assert!(!drain.eof());
    drain.finish();

    assert!(drain.eof());
    assert_eq!(drain.counters().frames, 0, "a torn tail is never a frame");
    assert_eq!(
        drain.feed(&frame[43..]),
        0,
        "a finished drain rejects the tail"
    );
    assert_eq!(drain.feed(&frame), 0, "and every later frame");
    assert!(drain.take_armed().is_none());
    drain.finish(); // idempotent
}

#[test]
fn empty_chunks_never_move_the_scanner() {
    let drain = PipeDrain::manual();
    assert_eq!(drain.feed(&[]), 0);
    assert_eq!(drain.counters().frames, 0);
    assert!(!drain.eof());
}

#[test]
fn state_digest_reassembles_across_single_byte_feeds() {
    let digest = StateDigest::new([0xA5; 64]);
    let drain = PipeDrain::manual();
    for frame in digest.to_frames() {
        for byte in frame.to_bytes() {
            drain.feed(&[byte]);
        }
    }

    assert_eq!(drain.counters().frames, 3);
    assert_eq!(drain.take_state(), Some(digest));
    assert!(drain.take_armed().is_none(), "state is never an arm frame");
}

#[test]
fn token_sink_round_trips_through_feed() {
    let mut bytes = Vec::new();
    ProcessToken::simple(5, 6, 7)
        .write_to(&mut bytes)
        .expect("write arm");
    ProcessToken::commit(5)
        .write_to(&mut bytes)
        .expect("write commit");
    assert_eq!(bytes.len(), 88);

    let drain = PipeDrain::manual();
    assert_eq!(drain.feed(&bytes), 2);
    let counters = drain.counters();
    assert_eq!(
        (counters.frames, counters.arms, counters.commits),
        (2, 1, 1)
    );
    assert!(drain.take_armed().is_none(), "commit settled the arm");
}

#[test]
fn state_digest_sink_round_trips_three_frames() {
    let digest = StateDigest::new([3u8; 64]);
    let mut bytes = Vec::new();
    digest.write_to(&mut bytes).expect("write digest");
    assert_eq!(bytes.len(), 3 * 44);

    let drain = PipeDrain::manual();
    assert_eq!(drain.feed(&bytes), 3);
    assert_eq!(drain.take_state(), Some(digest));
}

static LEASE_MS: AtomicU64 = AtomicU64::new(100);

fn lease_clock() -> u64 {
    LEASE_MS.load(Ordering::SeqCst)
}

#[test]
fn injected_clock_drives_the_lease_without_wall_time() {
    let drain = PipeDrain::manual_with_clock(lease_clock);
    let frame = ProcessToken::simple(1, OP_HEARTBEAT, 0).to_bytes();

    LEASE_MS.store(100, Ordering::SeqCst);
    drain.feed(&frame);
    assert_eq!(drain.now_ms(), 100);
    assert_eq!(drain.idle_ms(), 0);

    LEASE_MS.store(105, Ordering::SeqCst);
    assert_eq!(drain.idle_ms(), 5);
    assert!(!drain.expired(5), "idle == lease is still inside");
    assert!(drain.expired(4));

    LEASE_MS.store(106, Ordering::SeqCst);
    assert_eq!(drain.idle_ms(), 6);
    assert!(drain.expired(5));

    drain.finish();
    assert!(!drain.expired(5), "EOF is not a lease breach");
}

#[test]
fn silent_drain_is_never_expired_before_its_first_frame() {
    static QUIET_MS: AtomicU64 = AtomicU64::new(0);

    fn quiet_clock() -> u64 {
        QUIET_MS.load(Ordering::SeqCst)
    }

    let drain = PipeDrain::manual_with_clock(quiet_clock);
    QUIET_MS.store(10_000, Ordering::SeqCst);
    assert_eq!(drain.idle_ms(), 0, "no frame seen yet");
    assert!(!drain.expired(1));
}

#[test]
fn spawn_with_clock_stamps_frames_from_the_reader_thread() {
    static SPAWN_MS: AtomicU64 = AtomicU64::new(42);

    fn spawn_clock() -> u64 {
        SPAWN_MS.load(Ordering::SeqCst)
    }

    let frame = ProcessToken::simple(1, 9, 1).to_bytes().to_vec();
    let drain = PipeDrain::spawn_with_clock(Cursor::new(frame), spawn_clock);

    let deadline = Instant::now() + Duration::from_secs(2);
    while !drain.eof() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }

    assert!(drain.eof(), "the reader must reach EOF");
    assert_eq!(drain.counters().frames, 1);
    assert_eq!(drain.idle_ms(), 0, "stamped with the injected clock");
    assert_eq!(drain.take_armed().map(|token| token.task), Some(1));
}

#[test]
fn commit_and_heartbeat_never_arm() {
    let drain = PipeDrain::manual();
    let mut chunk = ProcessToken::commit(1).to_bytes().to_vec();
    chunk.extend_from_slice(&ProcessToken::simple(0, OP_HEARTBEAT, 0).to_bytes());

    assert_eq!(drain.feed(&chunk), 2);
    let counters = drain.counters();
    assert_eq!(counters.arms, 0);
    assert_eq!(counters.commits, 1);
    assert!(drain.take_armed().is_none());
}

#[test]
fn state_chunks_do_not_arm_and_corrupt_chunks_are_dropped() {
    let digest = StateDigest::new([9u8; 64]);
    let frames = digest.to_frames();

    let drain = PipeDrain::manual();
    let mut torn = frames[0].to_bytes();
    torn[44 - 1] ^= 0xFF; // break the chunk CRC
    assert_eq!(
        drain.feed(&torn),
        1,
        "the frame parses, the chunk is invalid"
    );
    assert_eq!(drain.feed(&frames[1].to_bytes()), 1);
    assert_eq!(drain.feed(&frames[2].to_bytes()), 1);

    assert_eq!(drain.counters().arms, 0);
    assert!(
        drain.take_state().is_none(),
        "a torn chunk must not complete a digest"
    );
    assert!(drain.take_armed().is_none());
}

#[test]
fn opcodes_stay_distinct_for_feed_dispatch() {
    assert_ne!(OP_COMMIT, OP_HEARTBEAT);
    assert_ne!(OP_COMMIT, OP_STATE);
    assert_ne!(OP_HEARTBEAT, OP_STATE);
}

#[test]
fn snapshot_marker_neither_arms_nor_clears() {
    let drain = PipeDrain::manual();

    assert!(
        drain.take_armed().is_none(),
        "a lone snapshot marker is diagnostics, not an armed token"
    );
    assert_eq!(
        drain.feed(&ProcessToken::simple(7, OP_SNAPSHOT, 0).to_bytes()),
        1
    );
    assert_eq!(drain.counters().arms, 0, "OP_SNAPSHOT arms nothing");
    assert!(drain.take_armed().is_none());

    // It must not clear an existing armed transaction either.
    assert_eq!(drain.feed(&ProcessToken::simple(11, 5, 9).to_bytes()), 1);
    assert_eq!(
        drain.feed(&ProcessToken::simple(3, OP_SNAPSHOT, 0).to_bytes()),
        1
    );
    let armed = drain
        .take_armed()
        .expect("emergency token survives a snapshot marker");
    assert_eq!(
        armed.task, 11,
        "the snapshot frame must not replace the armed token"
    );
    assert_eq!(drain.counters().arms, 1);
}
