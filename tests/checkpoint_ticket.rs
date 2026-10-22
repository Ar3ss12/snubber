//! The Ticket Law, verified in-process: a 64 B `StateDigest` certifies one
//! exact, already-sealed file. Wrong magic, unknown version, size mismatch
//! or body CRC mismatch makes the ticket dead — the successor rolls back.
//!
//! Shares `examples/_shared/checkpoint.rs` with example 16, so the test
//! exercises the same seal/verify code the demo ships.

#[path = "../examples/_shared/checkpoint.rs"]
mod checkpoint;

use checkpoint::{
    make_state, read_ticket, restore, seal, tmp_path, verify, write_checkpoint, write_ticket,
    TicketError, FILE_ID, HARD_KB, RSS_AT_SOFT_KB, RSS_NAIVE_KB, RSS_STREAM_KB, SOFT_KB,
};
use snubber::{DeathCause, MemoryPolicy, MemoryVerdict};
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("snubber_ckpt_{tag}_{}.sncb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(tmp_path(&path));
    let _ = std::fs::remove_file(path.with_extension("ticket"));
    path
}

#[test]
fn ticket_roundtrip_and_dead_ticket_matrix() {
    let state = make_state(32 * 1024);
    let path = scratch("matrix");
    let crc = write_checkpoint(&path, &state).expect("seal");
    let digest = seal(7, 3, state.len() as u64, crc, &FILE_ID);

    assert_eq!(
        verify(&digest, &path).expect("fresh ticket"),
        state.len() as u64
    );
    assert_eq!(restore(&digest, &path).expect("restore"), state);

    let mut bad = digest;
    bad.bytes[0] = b'X';
    assert!(matches!(verify(&bad, &path), Err(TicketError::DeadMagic)));

    let mut bad = digest;
    bad.bytes[4] = 2;
    assert!(matches!(
        verify(&bad, &path),
        Err(TicketError::DeadVersion(2))
    ));

    let over = seal(7, 3, state.len() as u64 + 1, crc, &FILE_ID);
    assert!(matches!(
        verify(&over, &path),
        Err(TicketError::SizeMismatch { want, got })
            if want == state.len() as u64 + 1 && got == state.len() as u64
    ));

    // One flipped byte after the seal: size matches, CRC does not.
    let mut corrupted = state.clone();
    corrupted[1234] ^= 0xFF;
    std::fs::write(&path, &corrupted).expect("corrupt body");
    assert!(matches!(
        verify(&digest, &path),
        Err(TicketError::CrcMismatch { .. })
    ));
    assert!(restore(&digest, &path).is_err());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn torn_write_leaves_no_sealed_file_and_no_ticket() {
    let state = make_state(16 * 1024);
    let path = scratch("torn");
    // The writer was killed mid-`write_all`: only a partial `.tmp` exists.
    std::fs::write(tmp_path(&path), &state[..1000]).expect("partial tmp");

    let digest = seal(1, 1, state.len() as u64, 0, &FILE_ID);
    assert!(matches!(verify(&digest, &path), Err(TicketError::Io(_))));
    assert!(!path.exists(), "never renamed");

    let ticket = path.with_extension("ticket");
    assert!(read_ticket(&ticket).expect("absent ticket").is_none());
    std::fs::write(&ticket, [0u8; 63]).expect("short ticket");
    assert!(
        read_ticket(&ticket)
            .expect("short ticket reads as none")
            .is_none(),
        "a 63 B file is not a ticket"
    );

    let _ = std::fs::remove_file(tmp_path(&path));
    let _ = std::fs::remove_file(&ticket);
}

#[test]
fn sealed_file_wins_over_stale_tmp() {
    let state = make_state(8 * 1024);
    let path = scratch("wins");
    let crc = write_checkpoint(&path, &state).expect("seal");
    let digest = seal(2, 9, state.len() as u64, crc, &FILE_ID);

    // A stale or torn `.tmp` from the next (killed) generation must not be
    // mistaken for the checkpoint.
    std::fs::write(tmp_path(&path), b"torn").expect("stale tmp");
    assert_eq!(restore(&digest, &path).expect("sealed body"), state);

    let ticket = path.with_extension("ticket");
    write_ticket(&ticket, &digest).expect("persist ticket");
    assert_eq!(
        read_ticket(&ticket).expect("ticket").map(|d| d.bytes),
        Some(digest.bytes)
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(tmp_path(&path));
    let _ = std::fs::remove_file(&ticket);
}

#[test]
fn level_budget_separates_streaming_from_naive() {
    // `hard - soft` is allocator headroom, not a countdown.
    let mut streaming = MemoryPolicy::new(SOFT_KB, HARD_KB);
    assert_eq!(streaming.note(RSS_AT_SOFT_KB), MemoryVerdict::Soft);
    assert!(!streaming.should_kill());
    assert_eq!(streaming.note(RSS_STREAM_KB), MemoryVerdict::Soft);
    assert!(
        !streaming.should_kill(),
        "a 64 KiB stack buffer fits in the headroom"
    );

    let mut naive = MemoryPolicy::new(SOFT_KB, HARD_KB);
    assert_eq!(naive.note(RSS_AT_SOFT_KB), MemoryVerdict::Soft);
    assert_eq!(naive.note(RSS_NAIVE_KB), MemoryVerdict::Hard);
    assert!(naive.should_kill());
    assert_eq!(naive.death_cause(), Some(DeathCause::MemoryLimit));
}
