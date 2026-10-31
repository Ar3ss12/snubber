# Mathematical Invariants & Verification Protocol

`snubber` is designed around strict deterministic guarantees. Unlike best-effort resilience wrappers, its core state machine enforces formal bounds on resource allocation, concurrency limits, and state transitions under abnormal termination (including panics, thread cancellation, and external process kills).

---

## 1. Core Invariants

The state machine of `snubber` guarantees that the following thirteen invariants hold unconditionally under Safe Rust (Invariants 1–4 are the core machine; 5–13 extend the guarantee to clocks, locks, telemetry, impedance, reclaim, parsing, API evolution and async liveness):

### Invariant 1: Conservation of Admissions
At any point in time, the sum of all terminal and active states equals the total number of attempted admissions:

$$\text{Attempts} = \text{Admitted} + \text{BulkheadFull} + \text{BreakerOpen} + \text{StaleEpoch} + \text{RateLimited} + \text{InvariantBreach}$$

where `BulkheadFull` folds the async `QueueFull` verdict in (the queue is part of the bulkhead), and async `QueueTtlExpired` attempts settle as `Timeout` (a timed-out attempt, not a shed one). Memory-pressure refusals are counted **before** the perimeter in their own lock-free column and are therefore not part of the perimeter equation. The same law covers re-entrant admission from a node's own rollback thread: those attempts are mirrored in `rollback_phase().reentries` before the perimeter; under `RollbackReentrancy::Refuse` they are refused there (`EnterError::RollbackReentrant`) and stay outside the equation, while under the default `Allow` they continue into the perimeter and are counted by the usual columns too. No request is silently dropped, double-counted, or lost during state transitions, even under high-concurrency races.

### Invariant 2: Strict Slot Containment (Zero Leaks)
For any `SnubberNode` configured with capacity $C$:

$$0 \le \text{in\_flight}(t) \le C \quad \forall t$$

with one deliberate, documented exception: under `OverflowPolicy::EvictOldest` a saturated node overbooks by one ($C + 1$) to admit the newest caller; the displaced owner's slot is released by its own settlement (implicit drop → `EvictedOldest` column), so the overbooking is transient and never stacks. The $C + 1$ ceiling is enforced at admission (fixed in 0.6.4): a caller arriving while the overbook slot is already taken is refused with `BulkheadFull` — a second overbook would be erased from `in_flight` by the hard slot clamp while the live guard kept its real slot, desynchronizing `lane_in_flight` from `in_flight` and permanently disabling the preemption victim predicate (policy-dependent: `lane_in_flight[0] = in_flight` under the default `BackgroundOnly`, an empty `lane_in_flight[senior..]` slice under `StrictSenior`).

When all caller threads or async tasks complete (whether through `commit()`, explicit `abort()`, implicit RAII drop, or panic unwinding), the active lease count strictly returns to zero:

$$\lim_{t \to \infty} \text{in\_flight} = 0 \quad (\text{quiescent state})$$

No orphaned slot or leaked permit can permanently reduce node capacity.

The erased path names its slot decision in the type. `probe_erased`,
`probe_erased_auto` and `enter_supervised_erased` return
`ErasedGuard<ProbeSlot>` (released at entry);
`enter_erased_holding` / `enter_erased_holding_auto` return
`ErasedGuard<HoldingSlot>` (booked until `commit()` / `abort()` /
`Drop`). `SlotKind` is sealed, so no third kind can claim either lifetime,
and one `arm_erased::<K>` reads `K::HOLDS_SLOT` — the returned type and the
slot decision are the same fact, not two that can drift. A signature that
needs the booked slot refuses a probe at compile time (`src/erased.rs`,
the two `compile_fail` fences).

### Invariant 3: Single-Shot Rollback Guarantee
For any acquired guard or lease:
- The registered rollback routine is invoked **at most once**.
- A successful `commit()` permanently disarms the rollback.
- An `abort()` or unwinding `Drop` triggers the rollback exactly once.
- Double-drop, nested unwinding, or re-entrant drops are mathematically unreachable.

### Invariant 4: Bounded Pipe Drain under Process Termination
In the multi-process supervisor (Layer 4), when a monitored child process terminates abnormally via `SIGKILL` or `TerminateProcess`:
- The supervisor guarantees reader termination within a strictly bounded window (`DEFAULT_EOF_GRACE_MS` = 250 ms; overridable per pool via `PoolConfig::with_eof_grace`).
- Pipe handles are closed without kernel deadlocks or thread starvation.
- The external reservation held on behalf of the child is released unconditionally.

### Invariant 5: Monotonic Hang Clock + Settled Spin Verdict
For any armed guard with a wall lease (`timeout_ms > 0`):
- The clock never moves backwards: NTP sync, DST change, or manual
  edits cannot un-expire a deadline or rewind `elapsed_ms`
  (boot-relative `Instant` on `std`, `MonotonicMs` tick on firmware —
  never `SystemTime`).
- `check_timeout` settles a blown wall lease exactly once (rollback runs
  once, `Timeout` policy fires once, slot returns once) with a verdict
  from pure `classify_stall` math: `SleepBlocked` when CPU stood still
  (syscall / `Condvar` / deadlocked `Mutex`), `CpuSpin` only when
  `cpu_delta >= cpu_budget_ms` (`0` = wall-only, never spin).

### Invariant 6: Zero-Nesting Locks (Deadlock Impossibility)
`SnubberNode`'s lock NEVER nests inside `TaskConduit`'s lock, and vice
versa. Cross-module calls happen only AFTER `drop(guard)` (open-call):
wakers fire outside the node lock, the conduit never arms guards.
Nested locks (ABBA) are the only remaining deadlock class, so the law
is enforced by `tests/lock_hierarchy.rs` as a grep-gate (doc-comments
stripped, code hits trip).
The same open-call discipline covers ROLLBACK bodies: every `rollback()`
call site runs with the ledger released, which is why re-entering the
node from a rollback returns instead of deadlocking (`scoped` refuses at
capacity with `BulkheadFull`, async parks as `Pending`, erased scopes are
granted because their slot was released at admission). The invariant is
about OUR lock; a rollback that re-takes a FOREIGN mutex the caller's
`work()` already holds hangs anyway: no counter books it, and only an
external `watchdog::poll_rollback_node` names the still-active body.
The open-call window itself is observable from any thread without the
lock: `SnubberNode::rollback_phase()` reads plain atomics (two writes per
body, a clock stamp only when `rollback_budget_ms` is armed) and names the
`epoch`/`task_id` whose slot the bulkhead is still holding —
`tests/rollback_guardrails.rs` pins the parked, overrun and unarmed cases.
The rollback half of the law is a static gate, not a hope:
`tests/lock_hierarchy.rs::rollback_bodies_never_run_under_the_node_lock`
scans every pinned invocation site (13, across `src/lib.rs`,
`src/erased.rs`, `src/node.rs`) and trips if any lock taken earlier in the
same function is still held when user code runs; the site inventory count
itself is asserted, so a renamed or reformatted call site fails the gate
instead of silently shrinking it.
An overrun of `rollback_budget_ms` is a named fault, not silence: the
body that RETURNED after its budget lands in
`reported[FaultKind::RollbackStall]`, `last_fault`, the flight ring and
the default `Warn` policy; a body that panics mints no verdict (an unwind
is not a measurement). Re-entrant admission is a policy, never a
deadlock: `RollbackReentrancy::Allow` (default) admits or refuses by the
usual rules and counts in `rollback_phase().reentries`, while `Refuse` returns
`EnterError::RollbackReentrant` BEFORE any lock on the per-thread marker
(thread identity exists only under `std`; the policy is inert on
`no_std`).

### Invariant 7: Pulse Lag Is Never Silent (Black-Box Trace)
Every missed `WorkerPulse` verdict recorded via
`SnubberNode::note_pulse_stall(lag_ms)` lands in the cold frame:
`SnapshotV1.pulse_stalls` / `max_pulse_lag_ms` (pulse fields since v3, 512 B kept) and
fires `AnomalyKind::PulseStall` (`pulse_stall`, ignores `min_sample`
like desync). A post-mortem dump answers "did it lag?" with
`[DIAG 0x… <node> pulse_stall <lag>%]` instead of "0 errors".

### Invariant 8: Memory Pressure Is a Refusal, Never a Fault
External RSS samples (`SnubberNode::note_memory_kb`, `MemoryPolicy::note`)
only move a `Copy` verdict; the crate never measures memory itself.
- Above `soft_kb` every new admission fails `EnterError::MemoryPressure`
  from ONE relaxed atomic load taken BEFORE the node mutex — a pressured
  node never contends with its sampler or a snapshotter, and in-flight
  guards keep running to settle (cooperative drain).
- The refusal has its own column (`memory_rejections`): it never enters
  the failure window, never moves `failure_streak`, and never trips the
  breaker. `retry_after_ms()` is a fixed 1000 ms (`Refusal::Backoff`).
- A hard kill exists only on the supervisor side, only at `used >= hard_kb`,
  and is stamped `DeathCause::MemoryLimit`; soft crossings never kill.
- `hard < soft` is clamped up to `soft`, so no configuration can skip the
  drain phase.
- `soft` is a level, not a deadline: `hard - soft` is the allocator's
  headroom, and the hard kill fires on the first sampler tick (10 ms) at or
  above `hard`. A checkpoint that allocates a second copy of the state does
  not get time to flush; it dies inside the allocation.
- `StateDigest` is a 64 B ticket, not storage. The crate guarantees one
  CRC32 over the 64 bytes and a 3-frame (24/24/16 B) transport where torn
  or mismatched frames are dropped; serialization, the fixed 64 KiB buffer,
  tmp + `sync_all` + `rename`, page-cache eviction (`posix_fadvise`) and
  validation of the file body are app-owned. A kill mid-write leaves no
  sealed file, hence no valid ticket.
- A leak is judged by its derivative, never its level: `MemoryTrend<N>::sample`
  records a sample and `MemoryTrend<N>::verdict` reports a growth only for a
  FULL ring whose every sample beats its predecessor, and one sample below
  its predecessor clears the window. A rising 2 MiB step is not a leak at any
  absolute size, and an oscillation is not evidence, so neither `soft_kb` nor
  a single high sample can be dressed up as one. `N` and the trip mark are
  caller policy; `N = 0` and `trip_kb == 0` are disabled, not "zero bytes".
- One type per concept: the ring is crate-internal. The public owners are
  `MemoryTrend<N>` (bytes) and `HandleWatch<N>` (counts), and the one public
  answer is `MemoryReport { level, trend }` — a `MemoryVerdict` and a
  `LeakClassification`. Both halves are computed on every call, so the level
  can never mask the trend (0.7's single verdict reported `Soft` before it
  read the window); `is_pressured()` is the OR, `code()` picks the more
  urgent half. A leaf is `Ok`, which is the truth: the process is under both
  limits, it is only going up. Rates are computed only from a span the
  caller supplied (`interval_ms > 0`, `N >= 2`) — otherwise `None`, never
  invented.
- `LeakClassification::Sawtooth` is the honest gap in that rule: a window
  that climbs and drops never trips a monotonic mark, and the shape is
  named instead of invisible. Naming it is not a verdict — the crate says
  what the window looks like, the caller decides whether that is a fault.
- `LeakClassification::Advisory` is the early half of the same honesty and
  still not a verdict: a partial window that rose for at least three
  samples and at least half of `N`, whose growth projected across a full
  window would cross the trip mark. Only a full window may latch, so
  `is_rising()` / `is_leaking()` / `is_pressured()` are all blind to it by
  construction; its rate divides by the intervals it actually holds
  (`samples - 1`), never by `N - 1`.
- A leaked handle is invisible to RSS, so `HandleWatch<N>` applies the same
  derivative to an externally sampled count (handles, fds, sockets). An
  unreadable probe returns `None` and records nothing: substituting `0`
  would be a false dip that clears the window and disarms the detector on
  the one host whose probe failed. The crate never reads a count itself —
  same seam as `note_memory_kb`, one caller-supplied closure.

### Invariant 9: Impedance Is Explicit (One Slot, Two Walls)
Mismatched producer/consumer rates are handled by named primitives, never
by an accidental unbounded queue.
- `ConflateSender` retains at most ONE unsampled value. A publish into an
  occupied slot replaces it and increments `conflated`; `publish` returns
  that fact. Over any window
  `published == taken + conflated + occupied`, where `occupied` is 0 or 1.
  The counter is a monotonic relaxed load, the payload carries no `Clone`
  bound, and a producer never blocks on the consumer.
- `ExecutionEnvelope` is a 16 B `Copy` header: min/max wall µs and min/max
  payload bytes. `0` disables one bound; `max < min` raises the ceiling to
  the floor; bounds are inclusive. The wall half is judged only for
  profiled guards (`work_wall_us`); the payload half is exact for any
  guard from the caller's byte count.
- A floor or ceiling miss ticks `envelope_underflows` /
  `envelope_overflows` and nothing else: it never feeds the breaker, never
  changes `SnubberEvent`, never enters the 512 B `SnapshotV1` frame, and
  never turns a committed transaction into a failure.
- The harness is deterministic: the same seed produces the same payload
  sequence, and an injected clock produces exact microseconds. A burst is
  a non-overlapping run of `burst_len` consecutive samples strictly above
  the median.

### Invariant 10: Complete Resource Reclaim on Settlement (Zero-Leak Gate)

Every settlement path — `commit()`, `abort()`, `check_timeout()`, or an
implicit `Drop` — MUST deallocate every owned guard field: the
`Arc<Mutex<NodeInner>>` reference, the captured rollback closure and any
boxed profile state. Disarming the rollback by calling `mem::forget(self)`
is an architectural defect: it suppresses drop glue for the whole struct,
so the `Arc` refcount never returns to 1 and the node's backing allocation
outlives every transaction.

- Settlement state lives in an internal `settled: bool` flag plus
  `Option::take()` on single-owner fields. No settlement path may bypass
  field destruction; the guard leaves scope normally and the compiler emits
  full drop glue.
- Net heap delta per settled transaction is exactly **0 bytes**: 100,000
  `enter_auto` → `commit` cycles followed by the node's drop must restore
  the allocator baseline (`tests/guard_alloc_budget.rs`). The same gate
  covers Layer 1: 250,000 `SnubberGuard` / `ErasedGuard` settle cycles
  whose closures own a heap payload, including node erased commit and abort
  cycles, restore the thread's heap baseline.
- Reintroducing one `mem::forget(self)` shows up as a stranded
  `NodeInner` allocation (1,648 B in the reference build; measured with
  `CARGO_TARGET_DIR=target/sizecheck RUSTFLAGS="-Zprint-type-sizes"
  cargo +nightly-2026-10-09 rustc --lib --all-features` — the log also
  reads `Mutex<NodeInner>: 1,656 B`, `ArcInner<…>: 1,672 B`), not as a
  statistic drift — and gate 7/7 plus `#![deny(clippy::mem_forget)]`
  reject the call before it builds.
- Closure ownership is observable: `Arc::strong_count` on a captured
  handle must return to 1 after commit, abort, timeout and implicit drop,
  and the admission slot must be reusable (`tests/guard_settle_reclaim.rs`).
- The out-of-process harness enforces the runtime ceiling: supervisor
  private-memory growth above `--MaxGrowthMb` / `--max-growth-mb`
  (default 2.0 MB) fails the run even when every token was rescued.
- The multi-lineage harness gates per-lane handle growth
  (`-MaxHandleGrowth` / `--max-handle-growth`, default 16) alongside private
  growth; first, peak and last samples are reported so a one-off warm-up
  delta is visible. Descriptor sampling reads `/proc/<pid>/fd` on Linux and
  `lsof -nP -p <pid> -F f` on macOS, so the gate applies on both runners.
  Where neither source exists the check is reported as a skip, and a skip
  fails the verdict: reduced coverage is never a pass.
- Every reap is classified: **true ambush** (armed token left at death),
  **clean escape** (a commit was seen), or **stillborn** (no commit and no
  armed token). `-ArmedHold` / `--armed-hold` publishes the worker pid only
  after the first arm frame, so `-MinAmbushPct 70` / `--min-ambush-pct 70`
  proves the killer terminates armed workers instead of shooting during
  process startup.
- The armed slot is settled only by the commit that NAMES it: `OP_COMMIT`
  clears `DrainState::armed` when the frame's `task` equals the armed
  token's `task`, and leaves any other arm in place while still counting the
  frame. Clearing unconditionally let one transaction's commit erase another
  transaction's rescue token, which reported a genuine ambush as a clean
  escape and skipped the emergency handler. The slot holds ONE token: a
  second arm replaces the first, so only the last un-settled transaction of
  that pipe is rescued. Concurrent independent transactions therefore need
  one lane each (`WorkerPool`), and `task` ids must stay unique per
  un-settled transaction because the frame carries no generation.
  Tripwires: `a_commit_settles_only_the_arm_it_names`,
  `sequential_arms_settle_on_their_own_commit`,
  `a_lone_commit_neither_arms_nor_settles`,
  `a_second_arm_replaces_the_first_unsettled_token`,
  `a_foreign_commit_cannot_turn_an_ambush_into_an_escape`.
- The POSIX harness verifies process identity before `SIGKILL`: starttime
  plus image basename from `/proc/<pid>/stat` and `/proc/<pid>/exe` (Linux)
  or `ps -o lstart=,comm=` (macOS). A failed read is never cached, a killed
  pid drops its cached identity, and supervisor pids are excluded, so a
  recycled pid or a stale pid file cannot redirect a kill.
- In `WorkerPool`, every worker owns a private `PipeDrain`: a lane's
  `take_armed()` can only return a token emitted on that lane's pipe, so no
  rescue token crosses lanes.
- A lane's generation is bumped exactly once per reported death
  (`note_death` is the only bump site); no other lane's clock moves, and a
  reap hands the successor the post-bump generation.
- Pool conservation: `ambush + escape + stillborn + deadlocks == reaps` per
  lane and in the totals. `deadlocks` (no EOF inside `eof_grace_ms`) is a
  fourth honest bucket — never folded into the other three — and
  `PoolReport::healthy()` requires it to be zero.
- Restart math is per lane: one lane's crash storm (`GiveUp`) cannot stop,
  restart or delay another lane. The memory watch resets on `attach`, so a
  previous child's peak cannot euthanize its successor, and a reap or
  memory sample on an idle lane returns `NoWorkerAttached` instead of a
  silent zero.
- A lane lease (`expired(lease_ms)`) is measured from a frame's boot-relative
  timestamp; "no frame ever parsed" is tracked by a separate `seen` flag, so
  timestamp `0` (a frame parsed at process start) is not mistaken for
  silence. `hard < soft` clamps up, so the drain phase always exists.
- `WorkerPool::expired_lanes(lease_ms)` is a read-only sweep: it names a lane
  only when that lane's pipe is alive, was seen at least once, and has been
  silent past the lease. It takes each lane's state lock separately, never
  touches a child and never mutates counters, so a monitor thread can call it
  while every lane waiter is blocked in `child.wait()`.

### Invariant 11: Chunking-Independent Parsing (Feed Equivalence)
`PipeDrain::feed` is the same scanner as the reader-thread path, so frame
delivery cannot depend on how the transport sliced the stream:

- For any partition of the same byte stream, `feed` delivers the same
  sequence of `ProcessToken`s and the same `frames` / `arms` / `commits`
  counters as `drain_sync` over the whole stream.
- A partial frame is retained across calls: bytes are neither dropped nor
  duplicated at a chunk boundary. Bytes inside a 40-byte body are opaque
  (inserting garbage mutates that token, never starts a nested frame); the
  next `SNUB` magic re-synchronizes.
- `feed` returns exactly the number of frames completed by that call.
- `finish()` is the only EOF source for a manual drain: idempotent,
  thread-safe, discards a torn tail, and makes every later `feed` return
  zero without mutating state.
- The injected `ClockFn` is the only time source of a drain: `idle_ms`,
  `expired` and the pool EOF grace read the same counter, so a fake clock
  reproduces lease and deadlock verdicts without wall time.
- `ProcessToken::write_to` / `StateDigest::write_to` emit the identical
  bytes as `to_bytes()` / `to_frames()`, so a sink round-trip through
  `feed` yields the same counters and rescue classification as stdout.

### Invariant 12: Public API Extensibility Contract (0.6.0)

The public verdict surface evolves additively and honestly:

- Error/classification enums (`EnterError`, `FaultKind`, `AnomalyKind`,
  `ReapClass`, `RestartDecision`, `Directive`, `Refusal`, `GateError`,
  `MemoryVerdict`, `DeathCause`, `GatewaySendError`,
  `GatewayRecvError`, `PoolError`, `IsrPushError`, `StallKind`,
  `TimeVerdict`, `PayloadVerdict`, `SnapshotError`, `BreakerState`,
  `SnubberError`, `StageKind`, `TimeoutCheck`) are `#[non_exhaustive]`:
  a new variant is additive semver, and downstream matches must carry a
  `_` arm. Internal (defining-crate) matches stay exhaustive — the
  attribute binds only downstream crates, and a wildcard there would be
  unreachable code.
- New variants are APPENDED LAST. Declaration order is caller-visible
  through `as` casts, and `cargo-semver-checks` reports an insertion as
  `enum_no_repr_variant_discriminant_changed` (it caught
  `StallKind::RollbackStall` inserted before `DeadlockStall`, moving the
  latter from 3 to 4). `RollbackStall` is declared after `DeadlockStall`,
  and `tests/rollback_guardrails.rs` pins `[0, 1, 2, 3, 4]` locally so a
  machine without the semver tool still sees the break.
- Existing constructible structs are never extended. `SnubberStats` has
  public fields and `Default`, so downstream code can build it with a
  struct literal: an added field — public or private — is
  `constructible_struct_adds_field` (major), and `#[non_exhaustive]`
  would itself be breaking. New telemetry goes into a new
  `#[non_exhaustive]` view (`RollbackPhase`), never into an existing
  struct.
- Foreign hooks never run under the node lock. `friction_fn` /
  `friction_fn_ptr` / `pressure_fn` handlers are stored as
  `Arc<dyn Fn + Send + Sync>`, cloned (one atomic increment) under the
  lock and invoked after it is dropped; a handler may re-enter the node
  without deadlock.
- Node identity is dynamic without touching the `Copy` telemetry frames:
  `NodeName::{Static, Owned}` clones as a fat-pointer copy or one `Arc`
  increment; `SnapshotV1`, `MetricSample`, `Anomaly` and the wire
  headers keep `&'static str` (owned labels export the `<owned>`
  marker there).
- Format ceilings are published constants, not accidents:
  `Priority::COUNT` (3), `FaultKind::COUNT` (14),
  `PROFILE_STAGES` (4), `ISR_RING_CAP` (16),
  `TOKEN_PAYLOAD_LEN` (28) / `TOKEN_BODY_LEN` (40), `SNAPSHOT_LEN`
  (512), `DEFAULT_EOF_GRACE_MS` (250). Each is compile-asserted in the
  source; changing one is a wire-format version bump. `FaultKind::COUNT`
  sizes the `SnapshotV1::reported` tail, so a new kind is a snapshot
  version bump (v6 grew the tail to 14 for `RollbackStall`; the frame
  stays 512 B and older frames refuse as `BadVersion`).
- The cold frame carries the flight tape: `SNAPSHOT_TAPE_CAP` (6) cells at
  `SNAPSHOT_TAPE_OFFSET` (330), `len` u8 + `next` u8 + 6 × 24 B, ending at
  476 ≤ 508 and compile-asserted against both the CRC region and the flag
  offset. The tape is the one field the app fills; a decode clamps a
  hostile `len`/`next` to the ring instead of trusting the bytes.
- `Supervisor::..._argv(&[String])` variants run the supervisor loop on
  an explicit argument vector; without `--supervisor` or with the worker
  flag present they return `false` before spawning anything.

---

### Invariant 13: Parked-Waiter and Probe Liveness (0.6.1)
Every blocking mechanism in the crate must be WOKEN or DEREGISTERED, never
silently abandoned:
- Gateway depth: every successful send adds exactly 1, every take subtracts
  exactly 1 (signed arithmetic). After a full drain the counter is exactly 0
  and the throttle latch is clear — drift is impossible by construction.
- A parked `enter_async` waiter is woken by settlement OR removed by its
  future's `Drop`. The `EvictOldest` victim is woken and re-polls into
  `Err(EnterError::QueueFull)` — no zombie tasks, no wake/evict livelock.
- `check_timeout` on a half-open probe counts the timeout as probe failure:
  the breaker re-opens and the probe slot is released.
- `EpochGate` adopts a NEW generation unconditionally (epoch 0 included) —
  a rebooted client's first packet is never dropped.
- Conduit sync notifications (`resume`, `send_directive`) are issued while
  holding the `cancel_reason` mutex the waiter holds across its
  check-to-park window — a notification cannot land in the gap.
- A pool lane's generation bumps BEFORE its EOF grace wait: a successor
  attaching inside the window never inherits the dead child's generation.

Verification: `cargo test --test channel --test async_primitives --test circuit_breaker --test conduit --test supervisor_pool`

### Invariant 14: A Rollback Phase Is Never Silent When a Budget Is Armed

The node cannot cancel a rollback body (`#![forbid(unsafe_code)]`; no
library may kill a caller thread), so the guarantee is detect-and-report:
- While the body runs, `SnubberNode::rollback_phase()` answers from any
  thread with no lock and no allocation (`active`, `epoch`, `task_id`,
  `reentries`, and — when `rollback_budget_ms` is armed — `elapsed_ms`,
  `max_ms`, `stalls`).
- When the body RETURNS past its budget, the node books the overrun as
  `FaultKind::RollbackStall` (Invariant 6).
- While the body is STILL running past its budget,
  `watchdog::poll_rollback_node(node)` returns
  `RollbackStall { epoch, task_id, elapsed_ms, budget_ms }`, timed against
  the budget the node was built with (`node.rollback_budget_ms()`), so a
  watcher cannot drift from the builder. It is a pure read: it books no
  counter, appends no flight entry and mints no fault. The observer
  supplies the cadence; the node supplies the clock and the threshold.
  `poll_rollback(node, budget_ms)` keeps the 0.6 shape with the threshold
  from the caller, for an observer that outranks the builder.
- The clock has one owner: without `rollback_budget_ms` the phase reports
  `active` but `elapsed_ms == 0`, and neither form returns a verdict
  instead of inventing age. The `no_std` build keeps the same law; the
  observer itself lives under `std` (it needs no more than a thread the
  caller already has).
- The clock word has one writer class. `started_ms` is written only by a
  `begin` whose `active` increment found the count empty, and a closing
  body never touches it: a `fetch_sub` followed by a clear put a second
  writer on the same word and a concurrent `begin` could claim it between
  the two instructions, leaving the surviving body to report `active: true`
  with `elapsed_ms: 0` for its whole life — the exact wedge
  `watchdog::poll_rollback_node` exists to name. A stamp that outlives the
  body that took it is answered by the `active == 0` gate in
  `rollback_phase()`; while other bodies are still counted it belongs to
  the oldest claim, so `elapsed_ms` can over-report but never under-report.
  Per-body billing (`max_ms`, `stalls`) always uses that body's own start.
  Tripwires: `a_closer_leaves_the_clock_it_did_not_claim`,
  `concurrent_bodies_share_one_clock_a_closer_cannot_take`.
- Honest limit — the guarantee is a name, not a cure: nothing here
  prevents the wedge. `tests/rollback_guardrails.rs`
  (`wedged_rollback_wedges_the_node_forever`) proves a child process can
  name its own wedge from a second thread while the wedged thread never
  returns, and that the parent still has to kill it.

Verification: `cargo test --test rollback_guardrails` (plus the ignored
subprocess lane under `-- --ignored`).

### Invariant 15: Teardown Order Is a Type, Not a Loop Accident

A stop loop is the one place where iteration order is a correctness
property, and it is the one property a test of the loop cannot see: both
orders compile and both "pass". So the order is carried by the type:
- `RankedNode<'a, RANK: u8>` pins a node's stop position with a const
  parameter. Higher `RANK` is stopped first; the rank has no setter, so no
  call site can re-decide the position of a node that already declared one.
- `RankedSet<CAP>` inserts each member at its rank position (highest
  first, equal ranks in insertion order) and iterates front to back, so
  reading the stop order costs no sort and no allocation. A tie keeps
  registration order, because a tie means "independent" and any other rule
  would make the order depend on how many members preceded it.
- A full set returns `RankedSetError::Full` and changes nothing. It never
  evicts: the member that vanished is exactly the node the loop forgets to
  stop, which is a silent step loss rather than a visible refusal.
- The rank is data once admitted (`RankedEntry::rank`), because one set
  holds entries of different `RANK` types; the const parameter exists at
  the declaration site, where the topology is known.

Verification: `cargo test --lib rank` (ordering, tie stability, refusal).

### Invariant 16: A Preventive Rotation Hands Off Verified State, Never a Hybrid

A child can be retired before the hard wall only if the successor inherits
exactly what the retired child published — no more.
- `Rotation` opens one window on the first `Soft` sample and never extends
  it: later samples of the same window report the same `deadline_ms`, so a
  steady child is rotated on schedule, not on every tick. A sample below
  soft closes the window, so a spike that recovers never rotates.
- `Hard` is not a rotation trigger. The process is already over budget and
  `MemoryPolicy::should_kill` owns that path; a hard sample closes the
  window like any other non-soft sample.
- `Rotate` is a standing order until the caller records the new generation
  (`handoff` / `restart`), so a loop that misses a tick cannot read an
  overdue child as healthy.
- `max_rotations` bounds the lineage churn (`0` = unlimited). Once spent,
  soft is telemetry again and the hard wall is the only stop. An unplanned
  death (`restart`) never spends the budget.
- `handoff` consumes the last verified digest (`PipeDrain::take_state`) and
  bumps the generation in the same call: the successor enters at
  `retired + 1`, so every packet from the retired worker is stale. A torn
  (two-chunk) digest yields `None`, and `Supervisor::spawn_successor` then
  gives the successor a NULL stdin — an immediate EOF, never a hybrid
  state and never a block on the inherited terminal.

Verification: `cargo test --test rotation` (fake clock for the window, a
real successor process re-exec for the stdin handoff).

## 2. Verification Protocol

The continuous integration suite validates these invariants under sustained chaos testing:

```bash
# Verify admission conservation under heavy concurrency (8 threads x 5,000 cycles)
cargo test --test concurrency_stress

# Verify process death handling and supervisor pipe recovery
cargo test --test supervisor

# Verify the liveness witness rules (death outranks silence, None != dead, strict grace)
cargo test --test sentinel
cargo test --no-default-features --lib sentinel

# Verify bulkhead pushback and rate limits
cargo test --test bulkhead_capacity
cargo test --test circuit_breaker

# Verify monotonic hang layer (SPIN vs SLEEP + pulse)
cargo test --test clock_pulse

# Verify ISR bridge SPSC ring (no unsafe, M0-safe)
cargo test --lib isr

# Verify zero-nesting lock law (deadlock impossibility)
cargo test --test lock_hierarchy

# Verify the rollback phase, budget verdict, re-entrancy policy and outside watchdog
cargo test --test rollback_guardrails

# Verify pulse black-box (stall verdicts in the current-version snapshot + anomaly)
cargo test --test diagnostics pulse_stall

# Verify extensibility contract (hooks outside the lock, names, ceilings, argv)
cargo test --test extensibility

# Verify memory refusal + snapshot v7 gauges (soft refuses, hard verdicts)
cargo test --test memory_pressure
cargo test --test diagnostics the_flight_tape_survives

# Verify supervisor euthanasia decisions and the real soft-drain loop
cargo test --test supervisor_memory
cargo test --test supervisor_memory_loop

# Verify worker pool lanes (isolation, generation, conservation, deadlock grace)
cargo test --test supervisor_pool

# Verify the pool storm gate (4 lanes x 8 hard kills; ignored by default)
# --release, because the committed evidence in docs/evidence is release-profile.
cargo test --release --test supervisor_pool_stress -- --ignored --nocapture

# Verify all four reap classes in one run (armed/clean/silent/orphan; ignored)
cargo test --release --test supervisor_pool_stress_mixed -- --ignored --nocapture

# Verify lease euthanasia of heartbeat-then-hang workers with a live observer
cargo test --release --test supervisor_pool_stress_lease -- --ignored --nocapture

# Verify chunked parsing equivalence, injected clocks and token sinks (invariant 11)
cargo test --test drain_feed

# Verify bench hygiene (setup outside the hot loop, pure overhead only)
cargo test --test bench_hygiene

# Verify one-slot conflation (latest wins, exact coalescing, payload drops)
cargo test --test conflation

# Verify envelope laws (zero disables, clamp, underflow/overflow own columns)
cargo test --test execution_envelope

# Verify harness determinism (scripted clock matrix, payload range, PRNG)
cargo test --test harness

# Verify zero-leak settlement: Arc/closure reclaim + byte-neutral 100k cycles
cargo test --test guard_settle_reclaim
cargo test --test guard_alloc_budget

# Verify out-of-process kill path under armed-hold (true ambush >= 70%)
# Windows (PowerShell); POSIX equivalents: scripts/stress_harness.sh --launch --aggressive
# and scripts/stress_multi_lineage.py
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 1000 -Launch -RawKill -Aggressive -ArmedHold

# Verify N independent lineages under a parallel death storm (Windows, 4 x 2,500 kills)
# -LaneRoot gives each lane its own working directory, so the worker's
# relative pid file cannot be read by another lineage; -ReportPath /
# -StatsPath / -LogPath name the three artifacts one run writes.
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold

# The same 10,000 kills spread over 8 lineages — the saturation run
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 8 -KillsPerLine 1250 -Aggressive -ArmedHold

# Same storm on Linux/macOS (SIGKILL path, run by .github/workflows/stress.yml)
python3 scripts/stress_multi_lineage.py --lines 4 --kills-per-line 2500 --aggressive --armed-hold \
    --report target/multi_lineage_report.json --stats target/multi_lineage_stats.json \
    --log target/multi_lineage_run.log
```

### Gate taxonomy: semantic oracles vs drift tripwires

Two mechanisms protect this crate and they are not interchangeable:

- **Oracle** — a compiler, a real build, a real run or a committed
  measurement decides the guarantee (clippy lint, bare-metal compile,
  semver diff, `cargo test`, artifact comparison). Deleting the check's
  text does not weaken the guarantee; injecting the defect cannot pass.
- **Drift tripwire** — a text scan stands where no stable-Rust oracle
  mechanism exists. It is loud on drift (FAIL names the site) and
  guards *consistency*, not semantics.

`scripts/check_compile_gates.sh` / `.ps1` (7 gates, one `PARITY:` line per
flavor) and `tests/doc_sync.rs` classify as follows:

| Check | Guarantee | Class | Why this mechanism |
|---|---|---|---|
| Gate 1 — `#![forbid(unsafe_code)]` presence + `unsafe{` scan of src | No unsafe anywhere in the library | Tripwire around the real enforcement | Nothing at compile level tests an attribute's *absence*; the attribute IS the enforcement (any `unsafe` block fails the build) and the scan is the second contour that notices the attribute itself being removed |
| Gate 2 — arithmetic scan over the counter ledger | Stats counters never wrap | Tripwire with run-time-derived scope | A type-level oracle (newtyped counters) breaks the public `SnubberStats` API — rejected. The scan scope derives from `pub struct SnubberStats` fields + `in_flight` at run time, so a new ledger bucket is covered without editing either script (0.6.11) |
| Gate 3 — `format!`/`print*` scan of the three lock-path bodies | No allocation-for-formatting under the node mutex | Tripwire | No stable lint names "allocates while a lock is held"; bodies are extracted by function name and extraction failure FAILs the gate rather than skipping silently |
| Clippy denies in `src/lib.rs` (`unwrap_used`, `expect_used`, `panic`, `mem_forget`) | Panic-free library (Invariants 3 and 6 rest on it) and no forgettable guard | Oracle | Compiler-checked crate-wide since 0.6.11; every `#[allow]` is a reviewed site carrying its reason |
| Gate 4 — forbidden-metaphors grep (src, benches, examples, tests) | AGENTS.md §14 documentation register | Tripwire | Style policy — there is no semantic oracle for "what reads as marketing" |
| Gate 5 — core-only unit tests on the host + `thumbv7em-none-eabihf` compile + feature-shape anchors | The `no_std` layer, tests included, builds and passes without default features, and the library builds on bare metal | Oracle (the run and the compile) with tripwire anchors | The bare-metal build IS the gate; the host half runs the `#[cfg(test)]` blocks on both core shapes (`--no-default-features` and `--features alloc`), because those blocks compile against std in every default-feature run and a `Vec` in them failed only CI in 0.7.1. Missing toolchain = SKIP, and on CI SKIP = FAIL |
| Gate 6 — `cargo-semver-checks` against the last release | Public API honors semver | Oracle | Real API diff against the published baseline; on CI SKIP = FAIL |
| Gate 7 — `mem::forget` text scan | Guards are never leaked by `mem::forget` | Tripwire around the oracle (deny) | The clippy deny is the enforcement; the grep is redundancy for the lint's restrict-group blind spots |
| `doc_sync` — headings, taglines, layer column, module paths, anchors | Public docs agree with each other and with the source layout | Tripwire | Text policy of prose docs; no oracle outside the text exists. Fails per doc, per heading |
| `doc_sync` — bench numbers | Measured latencies live in exactly one doc and equal the committed Criterion snapshot | Oracle against artifact | `test_bench_numbers_match_evidence_artifact` checks every `~<number>` citation in BENCHMARKS.md against `docs/evidence/2026-10-30_bench_snapshot.md` (round-or-truncate), and README.md / docs/USER_GUIDE.md fail the suite if they quote any latency figure — they link instead of copying numbers; the guide's API & cost quick reference is a signature/footprint dictionary whose measured cost cells were replaced with structural costs (0.6.11). `test_bench_rows_quote_their_own_measurement` pairs each BENCHMARKS.md row with the snapshot row it names and requires the number to come from THAT measurement: an existence check alone let `pool.expired_lanes` publish another row's 5.4 ns while its own sweep measured 516 ns (2026-10-30) |
| `doc_sync` — release history | The User Guide describes the current state; release history lives only in CHANGELOG.md | Tripwire | `test_guide_has_no_release_history` rejects the history constructs in the guide — `Fixed in …` / `Fixed after …` headings, a version-labelled Extensibility heading, `v0.…` header teasers — because a guide that narrates past releases reads stale the day after release; the audit tables live in the CHANGELOG appendix (0.6.11) |
| `doc_sync` — storm numbers | The BENCHMARKS.md termination-storm tables equal the committed harness reports | Oracle against artifact | `test_storm_tables_match_committed_reports` rebuilds, for each of the three 2026-10-31 reports (armed-hold, free-running, 8-lane saturation), the mode row, the per-lane bullet, the histogram-band line, the killer-counter line, the machine caption and the rescue-accounting fragment, and requires an exact line match against `docs/evidence/2026-10-31_multi_lineage_*_report.json`. Before quoting anything it checks each artifact against itself: `verdict: PASS`, `report_schema: 2`, release profile, `checks == lanes × 11/10 + 4/3` (11 per lane under `-ArmedHold`, 10 without the ambush-rate gate) with none red, one `armed_hold matches flag` gate per lane, one `resource window readable` gate per lane, kills/bands/killer counters/`tokens_rescued` conserved across lanes, `final_generation == kills + 1`, zero stale pids, `tokens_rescued == true_ambush` and `tokens_rescued + clean_settles == kills_completed`. The reader is scoped (one key inside one object) because `dispatch_us`, `samples` and `final_generation` appear once in the aggregate and once per lane — a first-occurrence reader answers an aggregate question with lane 0's number. The citation is pinned in both directions: BENCHMARKS.md must link every artifact it quotes and must not name the superseded 2026-10-27 or 2026-10-30 set as current evidence (both stay committed as the record of their tree). The CI-runner rows cite one named workflow run and are read from its workflow annotations — a runner's report is not a repository artifact, and the table says so in its own caption |
| `doc_sync` — pool storm numbers | The three `supervisor_pool_stress*` rows equal their committed reports | Oracle against artifact | `test_pool_storm_tables_match_committed_reports` derives every pool row from `docs/evidence/2026-10-30_pool_*_stress_report.json` and re-checks the report's own accounting first: `ambush + escape + stillborn + deadlocks == reaped`, `rescued == ambush == tokens_seen`, `final_generation == reaped/lanes + 1` per lane, 0 `token_lane_mismatch`, 0 conservation breaks — plus the per-mode extras (lease: `lease_kills == reaped` and a monitor that actually ran, whose `monitor_checks` the prose quotes; mixed: `deadlock: true` with `deadlocks == reaped/lanes`, because an orphan leg is named rather than counted as a rescue). It then reads `docs/USER_GUIDE.md`: the guide must name the committed `2026-10-30_pool_` set and must not carry `wall_ms` or `monitor_checks` at all — a clock column copied into prose goes stale on the next run and nothing re-derives it, and the guide did cite the superseded debug set until 0.7.7 |
| `doc_sync` — one report schema for two harnesses | The PowerShell and Python storms emit the same keys, so one oracle reads a Windows and a POSIX artifact | Tripwire on the harness sources | `test_storm_harnesses_publish_one_report_schema` parses both report payloads indentation-scoped (nested lane blocks cannot join), requires ≥ 25 keys each, compares the shared prefix name by name, and pins the Python-only tail to exactly `platform,kills_dispatched,fd_sampling`; it also fails if either harness drifts back from the disjoint `band_*` histogram keys to a running-total `le_*` name. `test_both_storm_harnesses_gate_the_armed_hold_shape` requires one emission site of the `armed_hold matches flag` gate in each source: two sites emit two numbers and no oracle reads the extra, and a missing site lets `-ArmedHold` reach no supervisor while every other gate stays green |
| `doc_sync` — PowerShell sources are ASCII | Every `scripts/*.ps1` byte is ASCII, because the Windows runner decodes a BOM-less UTF-8 file as cp1252 | Tripwire on the bytes | `test_powershell_scripts_are_pure_ascii` reads each file as raw bytes instead of decoding, names file, count and first offending offset, and requires at least four files so a reflowed glob cannot pass vacuously; a local `Parser::ParseFile` decodes as UTF-8 and cannot see what the runner gets: an em dash's three bytes are read as cp1252 and end in U+201D, a quote character PowerShell honours |
| `scripts/check_test_count.sh` | The suite that ran is green, its parsed counts clear the floor, and README.md publishes no test count | Oracle | Parses `cargo test` output; fails on any failure, on a suspiciously low parse, or on a `tests-N passed` badge / "N passing tests" claim returning to the README (0.6.11: the vanity badge was deleted, so the gate guards its absence) |
| README snippets compile | Every runnable Rust block in README.md type-checks against the real API, so a pasted quick start cannot be stale | Oracle (the compile) with tripwire anchors | `cargo test --doc` compiles the fences through the `#[cfg(doctest)] #[doc = include_str!("../README.md")]` hook in `src/lib.rs` — the compiler is the oracle, so no assertion text is needed (0.6.11, after doc-bug H5: a README snippet carried a wrong arity). Two tripwires keep the mechanism itself from vanishing: `test_readme_is_wired_into_doctests` (a deleted hook would silently drop the README out of the doctest run) and `test_readme_fences_have_a_compile_verdict` (3 `rust,no_run` fences, `rust,ignore` capped at the one registered WITHOUT block, no modifier-free fence at all) |
| README assets resolve off-GitHub | The README renders identically on crates.io / docs.rs, not only on GitHub | Tripwire | `cargo package` excludes `assets/` and the registry pages have no repository tree behind them, so a relative `src="assets/…"` is a broken picture there; `test_readme_assets_are_absolute` requires an `https://` href and checks that a raw.githubusercontent.com path still exists in the tree at the named branch (0.6.11) |
| `rollback_guardrails` wedge canary | A rollback body that never returns is nameable from outside and never finishes | Oracle (real subprocess run) | The child arms `rollback_budget_ms`; a second thread of the child calls `watchdog::poll_rollback_node`; the parent requires the `RollbackStall` verdict BEFORE it kills the process. A child that only hangs fails the gate, so the assertion got stronger when the verdict arrived (0.6.12) |
| `lock_hierarchy::rollback_bodies_never_run_under_the_node_lock` | Rollback bodies never run under the node ledger lock | Tripwire (static scan, doc-comments stripped) | No stable lint names "a lock is held across this user callback"; the scan asserts its own site inventory (13), so a renamed or reformatted call site fails the gate instead of silently shrinking it (0.6.12) |
| `rollback_guardrails` phase tests | A running rollback is observable from any thread without the lock | Oracle (real run) | Real threads park a body; a second thread reads `epoch`/`task_id`/`elapsed_ms` and `watchdog::poll_rollback_node`, including the unarmed-clock refusal (0.6.12) |
| `rollback_guardrails::rollback_reentrancy_policy_refuse_returns_error` | Re-entrant admission is a per-thread policy: `Allow` counts, `Refuse` fails before any lock | Oracle (real run) | Sync arms a real bulkhead and async polls a real future; `classify()`/`backpressure()` pin the refusal class, and another thread is asserted unaffected (0.6.12) |

Two rules follow for the next refactor: do not add a hand-maintained list of
names or numbers to a tripwire when the list can be derived at run time
(Gate 2's counter scope, doc_sync's artifact rule are the precedents); and do
not delete a tripwire because it "only matches text" — where no oracle
exists for the guarantee, a loud drift alarm IS the protection.

### Test threshold convention: determinism, not percentages

Every volume assertion in a test must be tied to the mechanism the test
exercises, never to a share of total attempts:

```rust
// Right: each trip must be backed by real admitted work, and the refusal
// path the lane exists to prove must have fired.
assert!(admitted >= stats.breaker_trips.max(1));
assert!(stats.breaker_rejections > 0);

// Wrong: once a breaker trips, HalfOpen admits exactly one probe and
// refuses the rest, so admission volume is bounded by probes per trip.
// A percentage floor measures scheduler speed, not correctness:
// 0.6.9 shipped `admitted >= attempts / 10`, passed in debug, and failed
// in release CI (301 admitted of 12000, 75 trips, 11698 refusals).
```

Two corollaries:

- Release-mode lanes belong to the release gate. `bash scripts/release.sh`
  runs the `#[ignore]` pool storms, breaker churn and termination storm in
  `--release`; `.github/workflows/ci.yml` runs them on every push/PR, so a
  tag is never the first place they execute.
- Do not assert best-effort standard-library behavior that depends on
  backend codegen. `Waker::will_wake` compares vtable pointers; std relies
  on LLVM merging two identical const-promoted `RawWakerVTable`s, which
  Miri never does, so a cloned std `Arc` waker compares unequal under Miri
  only. Gate such asserts with `cfg(not(miri))` and say why.

### Test synchronization convention: handshake, not sleep margins

A test that needs another thread to free a resource must synchronize on an
observable event, never on "sleeping long enough":

```rust
// Wrong: bets that the commit lands inside the park window. A loaded
// runner can deschedule the committing thread for longer than the margin,
// after which the parked caller legally reports BulkheadFull (this is
// exactly how `block_caller_parks_then_admits_freed_slot` flaked).
std::thread::sleep(Duration::from_millis(10));
probe.commit();

// Right: the admission hook runs only when the bulkhead is full, so it
// proves the caller reached the park branch; blocking it until the holder
// commits makes the freed slot a certainty before the re-check.
reached_rx.recv_timeout(Duration::from_secs(5)).expect("parked");
probe.commit();
release_tx.send(()).expect("hook still waiting");
```

Bounded waits (`recv_timeout`, not `recv`) so a broken handshake fails the
gate instead of hanging the runner.

---

## 3. Reporting a Discrepancy

If you identify a scenario where any of the sixteen invariants above fails using purely Safe Rust:

1. **Minimal Reproduction:** Construct an isolated integration test demonstrating the violation (failing an explicit `assert!`).
2. **Deterministic Trigger:** The test must run and fail within 30 seconds without requiring external infrastructure.
3. **Submission:** Open an issue or Pull Request titled `invariant-violation: <invariant_name>` containing the test case in `tests/`.

Confirmed defects will be patched immediately with a dedicated regression test named after the reporter or organization.
