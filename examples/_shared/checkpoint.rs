//! Checkpoint helpers shared by `16_streaming_checkpoint.rs` and
//! `tests/checkpoint_budget.rs` / `tests/checkpoint_loop.rs`.
//!
//! This is APPLICATION code, not crate API: the 64 B layout is the protocol
//! recommended in `docs/USER_GUIDE.md` §4.6c. `snubber` transports the bytes
//! but never parses them, so the seal/verify rules live here.
//!
//! Everything is dependency-free: CRC32 (zlib), a fixed 64 KiB stack buffer
//! for the write, and a streaming CRC for verification — neither the write
//! nor the check allocates the body.
#![allow(dead_code)]

use snubber::{StateDigest, STATE_LEN};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Ticket magic: "Snubber ChecKpoint".
pub const MAGIC: [u8; 4] = *b"SNBK";
/// Layout version accepted by [`verify`].
pub const VERSION: u16 = 0x0001;
/// Flag bit 0 (payload app-defined; the example seals with flags = 0).
pub const FLAG_COMPRESSED: u16 = 1 << 0;
/// Flag bit 1.
pub const FLAG_ENCRYPTED: u16 = 1 << 1;
/// Flag bit 2: the body was written after an unclean shutdown.
pub const FLAG_DIRTY_RESUME: u16 = 1 << 2;
/// 16-byte app slot `[40..56]` used by the example.
pub const FILE_ID: [u8; 16] = *b"snubber-ckpt-v01";
/// Stack buffer reused for every chunk.
pub const BUF_BYTES: usize = 64 * 1024;
/// State size in the example / process tests (16 MiB).
pub const STATE_BYTES: usize = 16 * 1024 * 1024;

// Scenario budget shared by example 16 and the docs (KiB).
/// Drain threshold.
pub const SOFT_KB: u64 = 48 * 1024;
/// Kill threshold.
pub const HARD_KB: u64 = 56 * 1024;
/// Resident level at which soft fires.
pub const RSS_AT_SOFT_KB: u64 = 50 * 1024;
/// Streaming: one extra 64 KiB stack buffer.
pub const RSS_STREAM_KB: u64 = RSS_AT_SOFT_KB + (BUF_BYTES / 1024) as u64;
/// Naive: a full second copy of the state (`state.clone()`).
pub const RSS_NAIVE_KB: u64 = RSS_AT_SOFT_KB + (STATE_BYTES / 1024) as u64;

/// zlib-compatible CRC32: seed `0` for a fresh stream, or the previous
/// result to chain chunks.
#[must_use]
pub fn crc32(seed: u32, bytes: &[u8]) -> u32 {
    let mut crc = !seed;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Deterministic, compressible-enough body of `len` bytes.
#[must_use]
pub fn make_state(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// `.tmp` sibling used for the atomic rename.
#[must_use]
pub fn tmp_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

/// Pack the 64 B ticket (explicit little-endian, reserved/payload zeroed).
#[must_use]
pub fn seal(gen: u64, seq: u64, total: u64, crc: u32, file_id: &[u8; 16]) -> StateDigest {
    let mut d = [0u8; STATE_LEN];
    d[0..4].copy_from_slice(&MAGIC);
    d[4..6].copy_from_slice(&VERSION.to_le_bytes());
    d[6..8].copy_from_slice(&0u16.to_le_bytes()); // flags
    d[8..16].copy_from_slice(&gen.to_le_bytes());
    d[16..24].copy_from_slice(&seq.to_le_bytes());
    d[24..32].copy_from_slice(&total.to_le_bytes());
    d[32..36].copy_from_slice(&crc.to_le_bytes());
    d[36..40].copy_from_slice(&0u32.to_le_bytes()); // reserved
    d[40..56].copy_from_slice(file_id);
    // [56..64] stays zero
    StateDigest::new(d)
}

/// Stream `state` through one 64 KiB stack buffer into `.tmp`, `sync_all`,
/// then `rename` to `path`. Returns the body CRC32. Peak heap delta is
/// independent of `state.len()`; the seal is atomic.
pub fn write_checkpoint(path: &Path, state: &[u8]) -> std::io::Result<u32> {
    let tmp = tmp_path(path);
    let mut file = std::fs::File::create(&tmp)?;
    let mut buf = [0u8; BUF_BYTES];
    let mut crc = 0u32;
    for chunk in state.chunks(BUF_BYTES) {
        buf[..chunk.len()].copy_from_slice(chunk);
        crc = crc32(crc, &buf[..chunk.len()]);
        file.write_all(&buf[..chunk.len()])?;
    }
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(crc)
}

/// Persist the ticket next to the sealed file (the example does this after
/// `rename`; a real supervisor would keep it in `PipeDrain::last_state`).
pub fn write_ticket(path: &Path, digest: &StateDigest) -> std::io::Result<()> {
    std::fs::write(path, digest.bytes)
}

/// Read a ticket; a missing or wrong-sized file is `None` (dead ticket).
pub fn read_ticket(path: &Path) -> std::io::Result<Option<StateDigest>> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == STATE_LEN => {
            let mut d = [0u8; STATE_LEN];
            d.copy_from_slice(&bytes);
            Ok(Some(StateDigest::new(d)))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Why a ticket is dead. The successor treats every variant as rollback.
#[derive(Debug)]
pub enum TicketError {
    /// Not written by [`seal`] (or a false positive file).
    DeadMagic,
    /// Layout version this successor does not understand.
    DeadVersion(u16),
    /// The sealed file is not the size the ticket certifies.
    SizeMismatch { want: u64, got: u64 },
    /// Body bytes changed after the seal.
    CrcMismatch { want: u32, got: u32 },
    /// Missing or unreadable file (no sealed checkpoint).
    Io(std::io::Error),
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadMagic => write!(f, "dead ticket: bad magic"),
            Self::DeadVersion(v) => write!(f, "dead ticket: unknown version {v}"),
            Self::SizeMismatch { want, got } => {
                write!(f, "torn checkpoint: want {want} B, got {got} B")
            }
            Self::CrcMismatch { want, got } => {
                write!(f, "torn checkpoint: crc want {want:#010X}, got {got:#010X}")
            }
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for TicketError {}

/// All-or-nothing successor check: magic, version, exact size, body CRC.
/// Streams the file through the same 64 KiB buffer, so verification never
/// allocates the body. Returns the certified size in bytes.
pub fn verify(digest: &StateDigest, path: &Path) -> Result<u64, TicketError> {
    let d = &digest.bytes;
    if d[0..4] != MAGIC {
        return Err(TicketError::DeadMagic);
    }
    let version = u16::from_le_bytes([d[4], d[5]]);
    if version != VERSION {
        return Err(TicketError::DeadVersion(version));
    }
    let want_total = u64::from_le_bytes(d[24..32].try_into().expect("8 bytes"));
    let want_crc = u32::from_le_bytes(d[32..36].try_into().expect("4 bytes"));

    let mut file = std::fs::File::open(path).map_err(TicketError::Io)?;
    let got_total = file.metadata().map_err(TicketError::Io)?.len();
    if got_total != want_total {
        return Err(TicketError::SizeMismatch {
            want: want_total,
            got: got_total,
        });
    }

    let mut buf = [0u8; BUF_BYTES];
    let mut crc = 0u32;
    let mut read_total = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(TicketError::Io)?;
        if n == 0 {
            break;
        }
        crc = crc32(crc, &buf[..n]);
        read_total += n as u64;
    }
    if read_total != want_total {
        return Err(TicketError::SizeMismatch {
            want: want_total,
            got: read_total,
        });
    }
    if crc != want_crc {
        return Err(TicketError::CrcMismatch {
            want: want_crc,
            got: crc,
        });
    }
    Ok(got_total)
}

/// Verify, then load the body into memory (successor may afford it; a
/// worker under pressure must not call this).
pub fn restore(digest: &StateDigest, path: &Path) -> Result<Vec<u8>, TicketError> {
    verify(digest, path)?;
    std::fs::read(path).map_err(TicketError::Io)
}
