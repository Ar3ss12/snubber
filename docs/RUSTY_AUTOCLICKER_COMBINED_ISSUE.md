# Bug Report: Structural Panic in `start_autoclick` & Kernel Mouse Lock Under `panic = "abort"`

**Target Issue URL:** https://github.com/MrTanoshii/rusty-autoclicker/issues/new  
**Title:** `Potential kernel mouse lock under panic=abort + startup panic on clock offsets in start_autoclick`

---

### Markdown Body to Copy & Paste Directly into GitHub Issue:

```markdown
### Summary
While conducting a resilience and architectural safety audit of auto-clicker pipelines, we identified two structural edge-cases in `rusty-autoclicker` that can lead to crashes or persistent OS-level mouse button locks:

1. **Startup Panic on Large Time Offsets (`checked_sub().unwrap()`):**  
   In `src/app.rs`, calling `start_autoclick` with a click interval or start offset exceeding the process uptime triggers a panic inside `.checked_sub(...).unwrap()`.
2. **Kernel Button Lock Under `panic = "abort"` During Click-and-Hold:**  
   In release builds, `Cargo.toml` sets `panic = "abort"`, which suppresses RAII unwinding. If a panic or abort occurs while in `InteractionMode::Holding`, `Drop for RustyAutoClickerApp` never executes, leaving the physical mouse button held down indefinitely in the OS driver.

---

### Issue 1: Panic in `start_autoclick`

#### Location
File: `src/app.rs` (lines 307–311 in upstream `main`):
```rust
pub fn start_autoclick(&mut self, negative_click_start_offset: u64) {
    self.click_counter = 0u64;
    self.mode = InteractionMode::Autoclicking;
    self.rng_thread = rng();

    self.last_now = Instant::now()
        .checked_sub(Duration::from_millis(negative_click_start_offset))
        .unwrap(); // <-- Panics if negative_click_start_offset > process uptime
}
```

#### Mechanism & Steps to Reproduce
1. Start `rusty-autoclicker`.
2. Set click interval hours/minutes to any value larger than current process uptime (e.g. 1 hour = `3_600_000 ms`).
3. Press the activation hotkey immediately after launch.
4. `Instant::now().checked_sub(...)` evaluates to `None` because monotonic uptime has not reached that duration.
5. The unwrap fails:
   ```text
   thread 'main' panicked at src/app.rs:311:10:
   called `Option::unwrap()` on a `None` value
   ```
Because release builds specify `panic = "abort"`, the process crashes instantly.

#### Suggested Fix
Replace `.unwrap()` with `.unwrap_or_else(Instant::now)` so large offsets cleanly saturate to firing immediately instead of killing the process:
```rust
self.last_now = Instant::now()
    .checked_sub(Duration::from_millis(negative_click_start_offset))
    .unwrap_or_else(Instant::now);
```

---

### Issue 2: Kernel Mouse Lock Under `panic = "abort"`

#### Location
1. `Cargo.toml` (lines 31–37):
```toml
[profile.release]
opt-level = 3
lto = true
strip = true
panic = "abort" # <-- Suppresses stack unwinding and Drop destructors
```

2. `src/app.rs` (lines 345–351 in upstream `main`):
```rust
impl Drop for RustyAutoClickerApp {
    /// Release any button still held when the app shuts down, so quitting
    /// mid-hold never leaves a button stuck pressed at the OS level. (Does not
    /// run on a release-build panic, since `panic = "abort"`.)
    fn drop(&mut self) {
        if let Some(button) = self.held_button.take() {
            release_button(button);
        }
    }
}
```

#### Mechanism & Impact
As accurately noted in the code comment on line 347, `Drop` does not run when `panic = "abort"` is enabled. If any worker thread panics, an out-of-bounds error occurs, or an abrupt abort happens between `press_button` and `release_button`, the operating system driver retains the mouse button in a down state. This results in stuck drag-and-drop operations, accidental file moves, and unresponsive desktop UI until the user manually clicks their physical mouse.

#### Suggested Mitigation
* **Scoped RAII & Panic Boundary:** Run fallible input handling inside an isolated panic boundary (or worker thread with catch_unwind) to guarantee `release_button` executes before process teardown.
* **Supervisor / Watchdog:** If `panic = "abort"` is strictly required for binary size, a companion supervisor or atomic timeout lease ensures emergency hardware release on abnormal exit.

---

### Reference Implementation
We designed a lightweight zero-dependency boundary shim and transactional safety fuse addressing both invariant parameter validation and guaranteed hardware rollback across panics and process terminations:  
👉 [snubber on GitHub](https://github.com/Ar3ss12/snubber)

Hopefully this detailed audit helps improve `rusty-autoclicker`'s robustness!
```
