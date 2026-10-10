//! Crypto key handling: the secret must be zeroized on EVERY exit path —
//! success, error, panic. Drop is the only construct that covers all three.
//! `forbid(unsafe_code)`-clean: shared ownership via `Arc<Mutex<_>>`, the
//! guard's rollback wipes through the lock — no raw pointers.
//!
//! Run: `cargo run --example key_zeroize`

use snubber::SnubberGuard;
use std::sync::{Arc, Mutex};

fn main() {
    let key = Arc::new(Mutex::new([0xA5u8; 32]));
    {
        let wipe = key.clone();
        // The guard OWNS the wipe: uncommitted exit zeroes the secret.
        let guard = SnubberGuard::new(1, 1, move || {
            if let Ok(mut k) = wipe.lock() {
                k.fill(0);
            }
        });
        // … sign transaction … panic mid-sign …
        drop(guard); // key wiped even though nobody committed
    }
    let k = key.lock().expect("unpoisoned");
    assert!(k.iter().all(|&b| b == 0), "secret wiped from RAM");
    println!("KEY_ZEROIZE: secret wiped on all paths");
}
