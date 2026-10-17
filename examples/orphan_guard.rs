//! Orphan guard pattern (Phase 6): contract in crate, impl in APP.
//!
//! Run: `cargo run --example orphan_guard`
//!
//! The crate ships `OrphanGuard` trait + `NoOrphanGuard` only.
//! Win32 Job Objects need `unsafe` + `windows-sys`; unix prctl needs
//! `libc` - both forbidden by zero-dep + forbid(unsafe_code).
//! So the REAL backend lives HERE as osmotic pattern (copy-paste).
//!
//! Below: `NoOrphanGuard` demo (honest no-op) + the exact Win32
//! recipe as non-compiled docs. Never invent channel APIs here.
use snubber::{NoOrphanGuard, OrphanGuard};
/// Pattern: Win32 Job Object backend (needs `windows-sys`, NOT in crate).
/// ```ignore
/// use windows_sys::Win32::System::JobObjects::*;
/// pub struct Win32JobGuard { handle: isize }
/// impl snubber::OrphanGuard for Win32JobGuard {
///     fn arm(&self, pid: u32) -> bool {
///         // SAFETY (app owns): CreateJobObjectW + SetInformationJobObject
///         // with JobObjectExtendedLimitInformation +
///         // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, then AssignProcessToJobObject.
///         // Kernel reaps satellites when the parent dies - even kill -9.
///         let _ = pid;
///         true
///     }
///     fn backend_name(&self) -> &`static str { "win32-job-object" }
/// }
/// ```
fn main() {
    let guard = NoOrphanGuard;
    println!("ORPHAN: backend={}", guard.backend_name());
    // Honest no-op: returns false, caller logs + relies on outer reaper.
    assert!(!guard.arm(1234), "no-op promises nothing");
    println!("ORPHAN: arm(1234)=false, outer daemon owns reaping");
    // Recipe check: trait is object-safe, Send+Sync (supervisor holds it).
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NoOrphanGuard>();
    println!("ORPHAN: contract typed, app implements, crate stays clean");
}
