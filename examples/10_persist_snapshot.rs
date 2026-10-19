//! Pattern 10: Crash-safe snapshot persistence (app owns the filesystem).
//!
//! `snubber` never touches files: the app encodes a 512 B frame via
//! `SnapshotV1::encode()` and flushes it with atomic tmp+rename.
//! Boot path: `decode()` + `find_anomalies()` on the stored bytes.

use snubber::{find_anomalies, AnomalyPolicy, SnapshotV1, SnubberNode};
use std::path::Path;

/// Atomic snapshot flush: tmp write + rename (crash-safe on POSIX and NTFS).
fn persist_snapshot(path: &Path, snap: &SnapshotV1) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, snap.encode())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let dir = std::env::temp_dir().join("snubber_persist_demo");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("node.snub");

    let node = SnubberNode::builder("persist_demo").build();
    let _: Result<(), snubber::SnubberError<&'static str>> = node.scoped(|| {}, || Ok(()));
    let snap = node.snapshot_v1();
    persist_snapshot(&path, &snap)?;

    let bytes: [u8; snubber::SNAPSHOT_LEN] = std::fs::read(&path)?
        .try_into()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "short frame"))?;
    let back = SnapshotV1::decode(&bytes).expect("stored frame verifies");
    assert_eq!(back.committed, 1);
    let hits = find_anomalies("persist_demo", &node.stats(), &AnomalyPolicy::default());
    assert!(hits.is_empty());
    println!(
        "persist_snapshot: OK committed={} anomalies=0",
        back.committed
    );
    Ok(())
}
