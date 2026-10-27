# Changelog

All notable changes to `snubber`. Benchmarks live in [README.md](README.md);
the deeper contract tables live in [docs/USER_GUIDE.md](docs/USER_GUIDE.md)
("Fixed in …" sections) and [docs/INVARIANTS.md](docs/INVARIANTS.md).

## 0.6.11 (2026-10-27)

Audit-3 closeout (PLAN_AUDIT3 findings K1/K2, H1–H7, D1–D4; D5 was
dismissed after re-checking the examples). No API changes; the public
surface, wire format and telemetry layout are untouched.

### Fixed

- Panic-macro discipline is a compiler oracle now: `src/lib.rs` carries
  `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`, so
  an `unwrap`/`expect`/`panic!` in ANY library function — including
  helpers called while the node mutex is held — fails the clippy gate.
  The old Gate 3 text scan saw only three function bodies; its scans for
  those macros were removed (the remaining Gate 3 scan keeps the
  allocation/formatting half — `format!`/`print*` under the lock — which
  has no stable lint). The seven legitimate `expect` sites (protected
  resource invariant, pre-lock rollback take, async single-settle) carry
  `#[allow(clippy::expect_used)]` with their reason; the eight internal
  test modules allow the panics they deliberately exercise. MSRV 1.70
  library lanes (`cargo check --lib`, core/alloc/std) unaffected.

- Gate 2's counter scan scope is derived at run time from `pub struct
  SnubberStats` (every `pub u64` / `pub [u64; N]` field) plus `in_flight`,
  in both the POSIX and PowerShell flavors; the hand-maintained 15-name
  list it replaces had already drifted (`poisoned_rejected` is written by
  the node but was not listed, so a bare `+=` there passed silently). The
  derived set is 31 names — `PARITY: ... counters=31 ...` now follows the
  struct, so a new ledger bucket is covered without editing any script.
  Proven by injection: `pulse_stalls += 1` fails (the old list missed it),
  a fresh `pub fuzz_probe: u64` field with a bare `+=` fails with no list
  edit anywhere, both revert green.

- The duplicated bench literals left the docs: the README/GUIDE
  cross-check (`test_bench_numbers_agree_across_readme_and_guide`, 20
  hard-coded tokens) is replaced by
  `test_bench_numbers_match_evidence_artifact` in `tests/doc_sync.rs`,
  which compares both docs against the committed Criterion snapshot
  (`docs/evidence/2026-10-26_bench_snapshot.md`): every `~<number> ns/µs`
  citation must round or truncate to an artifact row (derived
  per-frame/per-lane figures and qualitative `~0` are skipped by rule),
  and a row cited by both docs must be cited with a common value. The
  old test went silent the moment both docs carried the same stale
  number — the new one fails that injection, and fails a one-sided
  wrong value too. Audit found nine stale citations (README `~94/~52/
  ~126` vs measured 93.3/51.2/125.1; GUIDE additionally `~8.0/~21.5/
  ~152/~546` vs 7.9/21.7/156.0/542.2); all docs now quote the artifact.

- The release-tag gate only accepts "marked Unreleased" when the
  `## Unreleased` heading sits above the version's own section with no
  other section heading between the two: a stale marker below the
  section (or inside an old section's body) no longer satisfies the
  gate. The `.ps1` compile-gates battery scans metaphors
  case-sensitively, matching the POSIX gate's `grep`.

- A guard with no watchdog armed takes no admission stamp
  (`needs_stamp`): `armed_at_ms` is written only when a deadline exists,
  and the passthrough dummy never reads the clock. `node.enter + commit`
  drops 124 → ~91 ns, the passthrough canary 96.5 → ~55 ns (50-sample
  Criterion runs, 2026-10-27). The documented cost: `FaultRecord::elapsed_ms`
  and `GuardSnapshot::elapsed_ms` are `0` for guards without a deadline;
  the timing contract is still pinned by a deadline-armed timeout test.
- The token bucket can no longer be refilled twice for the same wall
  time. `now_ms()` is read before the node mutex, so two admissions can
  reach the lock out of order; the old code then wrote the older stamp
  into `rate_last_ms`, and the next admission counted the interval
  again, admitting one extra burst of `rate_per_sec`. The refill now
  lives in one helper that only moves the stamp forward
  (`src/node.rs::refill_tokens`), shared by the sync and async paths and
  pinned by `bucket_stamp_never_moves_backwards` (fails on the old
  formula with `stamp rewound: 900 != 1000`).
- `EnterFuture` honours the documented eviction verdict: when the waker
  queue entry it owns has already been retired by an `EvictOldest`
  evictor, the next poll returns `Ready(Err(EnterError::QueueFull))`
  instead of re-parking into a fresh slot (which burned capacity and
  could evict a bystander). The entry flag is read under the lock that
  stamps it; a settle pop leaves it clear, so that re-park stays honest.
- The ISR shed counter saturates at `u16::MAX` instead of wrapping to
  zero — a shed count of 65,536 no longer reports "0 shed". Implemented
  as a `compare_exchange_weak` loop (no deprecated `fetch_update`, MSRV
  1.70 intact) and pinned by a 65,536-push test.
- Per-lane counters and admission hooks clamp with
  `Priority::MAX_INDEX` and `u32::try_from(..).unwrap_or(u32::MAX)`
  instead of the literal `min(2)` and an `as u32` truncation, so the
  bound follows `Priority::COUNT` and an over-large in-flight reading
  cannot wrap.
- The supervisor no longer clamps an out-of-range `OP_STATE` sequence id
  to chunk 2: a frame whose `task` is not 0/1/2 is malformed and is
  dropped, instead of overwriting the tail chunk and its authoritative
  full-digest CRC.
- `lock_poison_tolerant` documents why skipping `hard_clamp_slots` on the
  recovery path is safe (a foreign mutex poison cannot tear `NodeInner`;
  the holder finishes its own mutation, and `lock()` clamps on its own
  poison path).
- A `core`-only build is warning-clean again: `Priority::MAX_INDEX` was
  `dead_code` without `std`, so the const-assert block now pins
  `Priority::MAX_INDEX + 1 == Priority::COUNT` (the lane table's last
  dense index) and the constant is used in every feature combination —
  no `allow(dead_code)` waiver.
- Sync admission no longer reads the OS clock when the token bucket is
  disarmed. `7481e9a` hoisted `now_ms()` out of the mutex unconditionally
  to keep clock reads off the lock, which made every default admission
  pay one clock read; the bucket's arming is now mirrored outside the
  mutex (`SnubberNode::rate_per_sec`) and the stamp is taken only when a
  token is actually checked. Full 50-sample bench on the same machine:
  `node.enter + commit` 126.0 → 166.2 → 124.2 ns (baseline → regression →
  fix); `enter_erased_auto` 208 → 243 → 205 ns.

### CI and release gates

- The declared MSRV is now built. Every lane used `@stable`, so
  `rust-version = "1.70"` was an unverified claim; a new `msrv` job checks
  the library on Rust 1.70 in three shapes (core, core + alloc, std).
  Proven locally with `cargo +1.70 check --locked --lib` (all three
  green) and a negative probe: a temporary `LazyLock` fails on 1.70 with
  `E0658` (`lazy_cell` unstable). The guide now states the honest scope —
  the dev graph (criterion → clap) needs Rust 1.74+, so tests and benches
  are outside the floor and this lane deliberately does not use
  `-D warnings` (linting stays in the stable quality job; 1.70's
  dead-code lint disagrees on cfg-dependent items).
- `scripts/check_api_diff.ps1` no longer hard-fails when the baseline tag
  is HEAD (the release commit itself): it falls back to the previous tag,
  mirroring the `.sh` gate, and fails only when no previous tag exists.
  `check_compile_gates.ps1` Gate 6 delegates to that script instead of
  duplicating the rule.
- The stress harness refuses to run an external supervisor in CI
  (`CI=true/1` for `.sh` and `.py`; `.ps1` fails on any skip in every
  environment): without a launched child there is no pid for the
  RSS/fd/exit gates, so the run would report without leak evidence.
- `scripts/release.sh` no longer lets a tolerated semver SKIP hide a real
  gate failure. The verdict moved into `scripts/release_gate_verdict.sh`,
  which fails on any `FAIL:`/`FAILED` line or unexpected SKIP before it
  considers the local-only semver SKIP; a non-zero gate exit with no
  tolerated SKIP fails too, and a missing `PARITY:` fingerprint now stops
  the release instead of being grepped with `|| true`. Proven against
  crafted logs: semver-SKIP + `FAIL:` → rc 1, foreign SKIP → rc 1,
  opaque exit 1 → rc 1, tolerated SKIP alone → rc 0 with WARN.
- Gate 2 and Gate 7 of `scripts/check_compile_gates.sh` no longer abort
  on a clean scan. Both did `HITS="$(grep …)"; rc=$?` under
  `set -euo pipefail`: a zero-hit grep killed the script before `rc=$?`,
  so the `OK` branch (and the `PARITY:` line) never printed and the audit
  silently did its best impression of a passing gate. The scans now keep
  their exit code with `|| rc=$?`; proven by running the gates on a tree
  with every masking comment removed — both print `OK` and `PARITY:
  gates=7 counters=15 functions=3 metaphors=5` (before the fix the same
  tree exited 1 with no output at all).
- `termination-smoke` no longer hangs. The test used to launch the
  supervisor itself and then pass `--external-supervisor` (`.sh`) or
  nothing (`.ps1`), which tripped the CI `--launch` gate; the harness
  exited, the supervisor blocked in `child.wait()` waiting for kills that
  never came, and both matrix jobs burned the 20-minute job timeout. The
  test now spawns only the harness with `--launch` / `-Launch` (profile
  debug, matching its own build) and waits on it through a hard deadline,
  asserting its exit status — a refusing or wedged harness fails the lane
  in seconds. The dead `--external-supervisor` flag is removed from
  `stress_harness.sh`; kill-only mode still exists for a locally launched
  supervisor. Measured locally: 25 kills, verdict PASS, harness 0.9 s,
  lane 2.3 s (CI=true: 4.3 s) against the old 20-min cancel.
- The three Miri jobs compile with `RUSTFLAGS="-D warnings"` (on top of
  the existing `-Zmiri-strict-provenance`), so nightly-only lints fail
  the lane instead of scrolling past. `scripts/release.sh` now passes
  `--locked` to `cargo clippy` and `cargo publish --dry-run`, matching CI.
- The core-only lane now denies warnings. Gate 5 in
  `check_compile_gates.{sh,ps1}` and the two `--no-default-features`
  steps in `ci.yml` run under `RUSTFLAGS="-D warnings"`, so a warning on
  bare `core` fails the gate instead of printing and passing. The `.ps1`
  gate saves and restores the caller's `RUSTFLAGS`.

### Documentation

- Public docs no longer name things that do not exist: the bench label
  map and README use the real `success_rate_per_mille`; `rate_per_sec`
  documents its own `RateLimited` column instead of `BulkheadFull`;
  `queue_capacity` is labelled as the alias of the canonical
  `queue_limit`; the async entry module states the real waker-queue
  defaults (`queue_limit` 32, `RejectNew` — not "1024, oldest evicted");
  three dead `docs/FUTURE_TRAPS.md` pointers now cite
  `docs/INVARIANTS.md` (Invariant 3, Verification Protocol); and the
  `no_std` surface names `Protected` as core with `SafeLockExt` as
  `std`. Negative grep (`success_permille`, `FUTURE_TRAPS`,
  `1024, oldest evicted`) is empty; `cargo doc -D warnings` is clean.
- The `no_std` drain quickstart in `README.md` now compiles. It passed a
  `u64` tick value to `PipeDrain::manual_with_clock`, whose argument is a
  `ClockFn` (`fn() -> u64`); README fences are not doctested, so nothing
  caught it. The fixed snippet is duplicated as a doctest on
  `manual_with_clock`, and the badge moved to 428 (401 tests + 27
  doctests, 5 ignored).
- The three pool storm reports are committed under `docs/evidence/`
  (2026-10-26, with a provenance README). README and USER_GUIDE quote the
  committed files — `wall_ms=55/474/194`, 99,628 lease observer samples —
  and say which fields are deterministic counters and which are wall
  clock.
- README explains that `target/bench/snapshot.md` is rendered by
  `scripts/bench.sh` into the gitignored `target/` tree and archived by
  the `bench-full` CI lane; no copy lives in the repository.
- USER_GUIDE states the actual SKIP policies: platform-gated skips print
  and pass locally for `.sh`/`.py` but fail under `CI=true/1`; `.ps1`
  fails on any skip in every environment.
- The layer map is one document again. `src/lib.rs` now describes the
  same five layers as README and the User Guide (Layer 3 is the
  runtime-agnostic async engine, not the `alloc` extensions), states
  that `alloc` upgrades the existing layers in place rather than adding
  one, and the README rows for the supervised entry points, friction
  hooks and `NodeName` carry their real layer (4, 2, 2).
- `missing_docs` is `deny` instead of `warn`: the documented surface is
  already complete, so a new undocumented public item now fails the
  build in every feature combination instead of scrolling past as a
  warning.
- Every layer heading now carries the same canonical name in all three
  public docs (`Core RAII Guard`, `Concurrency Bulkhead`,
  `Runtime-Agnostic Async`, `Process Supervisor`, `Diagnostics &
  Health`); README's Layer 3 was still called "Async Waker Queue".
  `tests/doc_sync.rs` locks the bytes.
- The README benchmark tables are re-measured from two full 50-sample
  runs on 2026-10-26. The provenance paragraph records why the old table
  was not reproducible at its own commit (lock-free rows matched, guarded
  admission rows read 30–40% higher — the clock regression) and the
  contended rows quote full-run artifacts instead of a smoke check.
- The README table now publishes the rows that were measured but never
  listed (Layer 1.5 pulse/ISR, `TaskConduit`, `gateway`, `next_sequence`,
  `stats()`), and `scripts/bench.{sh,ps1}` map all 52 Criterion rows to
  operation labels instead of exporting 34 of them as snake_case names.

### Documentation

- The README latency table drops the `SnubberStats::shed_total` +
  `success_rate_per_mille` micro-bench row (two getter loads, no
  operation to choose); the bench and exporter labels stay. A `doc_sync`
  tripwire fails the suite if a public doc cites `success_permille` —
  a method that has never existed (the real one is
  `success_rate_per_mille`) — or if `src/node.rs` loses either real
  getter.

- The `no_std` clock-injection snippets no longer withhold the signature.
  The USER_GUIDE transport example called
  `PipeDrain::manual_with_clock(app_tick_ms)` with `app_tick_ms` and
  `uart_chunk` undeclared; it now declares
  `fn app_tick_ms() -> u64 { 0 }` and a byte chunk, and the comment names
  the type (`ClockFn = fn() -> u64, NOT a number`) in both docs. A
  `doc_sync` tripwire fails the suite when any public rust fence calls
  `manual_with_clock` without declaring `fn ...() -> u64` in the same
  snippet.

- Bench numbers no longer diverge between docs. The README prose
  (`attach + note_death` ~192/~173 ns, `report()` ~111 ns, lease sweep
  ~517 ns) and 14 USER_GUIDE rows (contended 925/899/146/34 ns, storm
  `wall_ms=486`, transition/priority/auto ~126/~126/~130 ns, gateway
  ~50.5 ns, conflation ~21.6/~16.0 ns, pool rows ~192/~173/~111/~517 ns)
  were stale against the 2026-10-26 Criterion run; every figure now
  quotes that run (`971/923/184/40` contended, `225/187` ambush/escape,
  `107` report, `519` expired_lanes, `474 ms` mixed storm, `90/87/94`
  post-`needs_stamp` entries). A new `doc_sync` tripwire pins 16 shared
  tokens across README and the guide and requires the four dual-site
  numbers (table + prose) twice per doc, so a one-site update fails the
  suite.

- The API & Cost Quick Reference table's `Layer` column is re-audited
  against the guide's own chapter map: 52 rows moved (hang/ISR/pulse
  rows to 1.5, memory/`StateDigest`/orphan/`RestartTracker` rows to 4,
  async admission and `ResumeFuture` to 3, the pool and drain rows to
  4, the §4.8 impedance-bridge rows — conflation, envelope, harness —
  to 4, `SafeLockExt` to 1/`std` (its `protect` module), diagnostics
  rows to 5). `ClosurePolicy` stays 2/`alloc` per the `src/lib.rs`
  layer map. A legend above the table defines the column, and a new
  `doc_sync` tripwire pins 23 representative rows to their layer so the
  column cannot silently rot again.

- The erased-guard bulkhead boundary is stated at preset level: the
  README gained a "Bulkhead Scope (erased guards)" guarantee entry —
  `max_in_flight(N)` bounds concurrent admissions, not live erased
  transactions (`enter_erased_auto`/`spawn_guarded` release the slot at
  admission) — the `enter_erased_auto` doc carries the same note, and a
  `doc_sync` tripwire fails the suite if either disappears.

- `docs/INVARIANTS.md` section 2 gained the "Gate taxonomy": a table
  classifying every compile gate, `doc_sync` check family and the
  test-count badge as either a semantic oracle (compiler, real build,
  real run, committed artifact) or a drift tripwire (text scan standing
  in where stable Rust has no oracle), each with the reason. The gate
  scripts and `tests/doc_sync.rs` headers point at the section. No gate,
  threshold or assertion changed; nothing was deleted — tripwires stay
  because where no oracle exists, a loud drift alarm IS the protection.

### Tests

- New `channel` test closes the senders and asserts the receive side
  settles as `GatewayRecvError::Disconnected`; the snapshot tamper test
  matches the exact `SnapshotError` variants (`BadCrc{want,got}`,
  `BadMagic`, `BadVersion(read)`) instead of a bare `is_err()`.
- Four formerly flaky suites are deterministic now: `checkpoint_budget`
  counts allocations thread-locally (a parallel test thread can no
  longer inflate another measurement's peak); `profiling` replaces the
  1 ms sleep handshake with an AtomicBool handshake;
  `async_gate`'s no-park bound is 500 ms; `affordances` joins the
  paused worker behind a 5 s `recv_timeout` (a deadlock fails in
  seconds, verified by a withheld-resume probe).
- New `isr_bridge` cross-thread FIFO test pushes 10,000 pulse signals
  from one thread through the 16-slot SPSC ring while another pops them,
  asserting opcode/reason/16-bit-sequence order end to end (Miri-scaled
  to 200 pushes; both loops bounded by a 30 s deadline so a lost or
  reordered signal fails instead of hanging). The pressure gauge asserts
  refused pushes, not losses. The suite ships executable coverage for
  the core branches at last: `cargo test --no-default-features --lib`
  and `--features alloc` (19 tests each) run in CI quality, and the
  suite joined the Miri concurrency lane.
- Four gates that could not fail now compare against an independent
  oracle. `conservation_model` drops the
  `admission_total() == started + shed_total()` tautology and checks the
  caller-side ledger instead (folding `evicted` into `shed_total` now
  fails 3 vs 2). `clock_park::no_wdt_feed_is_a_noop` pins a zero-sized
  `NoWdt` and a syscall-free call (a 2 ms sleep in `feed` fails).
  `stress_breaker_churn` replaces `reset_breaker(); assert Closed` with a
  dedicated `trip_after_failures(1)` node that trips, admits one probe and
  closes on commit (a probe that no longer closes reports `HalfOpen`).
  `doc_sync` locks a canonical one-line tagline per layer next to each
  heading in all three docs (deleting one fires the tripwire).
- `tests/state_handoff.rs::out_of_range_state_chunk_is_not_chunk_two`
  (an out-of-range `task` must not promote to chunk 2) and
  `src/isr.rs::shed_saturates_at_the_ceiling_without_wrap`.
- `tests/conservation_model.rs::mixed_policy_columns_conserve_every_attempt`
  drives `EvictOldest` overbooking, the `max + 1` bulkhead ceiling, a
  `Critical`-over-`Background` preemption request, a watchdog timeout,
  commit, planned abort and implicit drop through one node and compares
  every column against an explicit ledger:
  `admission_total() == total_started + shed_total()`, `evicted` and
  `preempted` excluded from the shed side, and `reported[]` verified as
  an alert ledger rather than a conservation term.
- `tests/doc_sync.rs::test_layer_map_is_identical_across_public_docs`
  requires each of the five canonical layer names to appear exactly once,
  byte-for-byte, in `src/lib.rs`, `README.md` and `docs/USER_GUIDE.md`,
  in ascending order; `test_documented_module_paths_exist` fails when
  prose cites a `src/...` module that is not on disk (it caught README's
  `Async Waker Queue` and now guards against stale paths like the old
  `src/async_entry.rs`).

## 0.6.10 (2026-10-26)

Audit-2 closeout (PLAN_AUDIT2 findings F-01..F-26; tag and publish only
on an explicit release order, so this section is documented-ahead of any
tag and the release-identity gate reports WARN until then).

### Fixed

- `SnubberGuard::abort` and `ErasedGuard::abort` now swallow a panicking
  rollback under `std` (`catch_unwind(AssertUnwindSafe(..))`), matching
  the documented contract and the `Drop` path: a planned abort never
  escalates into a double-panic process abort. Without `std` the
  behaviour is unchanged (a rollback panic during unwind aborts the
  process — a Rust rule, documented). `commit` is untouched.
- `PoolLane::note_death` no longer stamps `last_exit`/`last_class` onto a
  successor that attached while the reaper sat in the EOF grace (or after
  that successor's own life and reap advanced the generation). The death
  itself stays counted in the lane counters and returned in `PoolReap`;
  only the "latest" fields belong to the generation that actually died.
- The sync queue horizon is telemetry-only. When live admissions reach
  `queue_capacity`, the node records the `BufferOverflow` gauge
  (`buffer_overflow_total`, console line) but no longer executes
  `policy.action` on the admission that still succeeds. Previously a
  `TripBreaker`/`RollbackAndAlert`/custom policy armed on `BufferOverflow`
  tripped the breaker or latched the UI alert on successful admissions.
  The async `RejectNew` queue-full refusal is a genuine rejection and
  keeps the full policy, including `bulkhead_rejections`.
- Every automatic fault path (guard settle, async TTL timeout, async
  evict-oldest, admission refusals: bulkhead/stale/rate/breaker/poison)
  now bumps `reported[kind]` exactly once via `record_locked`, so
  `metric_samples()` and the console alert see `count >= 1` during the
  incident. The old split (only explicit `report()` and the rate/poison/
  preempt codes wrote the slot) left automatic faults invisible to both
  exporters. `bump_reported` is renamed `bump_column` and books only the
  mechanism column for explicit reports; explicit `report()` and the
  automatic path of the same kind now land on the same column.
- A named `report(FaultKind::BufferOverflow, …)` no longer credits
  `bulkhead_rejections` (a shed/conservation column). A BufferOverflow
  RECORD is capacity telemetry; the sync queue horizon raises it on an
  admission that still SUCCEEDS, so folding it into the shed columns
  broke the conservation law `admission_total() == total_started +
  shed_total()`. The async `RejectNew` queue-full refusal is a genuine
  rejection and still books `bulkhead_rejections`.
- `check_timeout` no longer bumps `reported[Timeout]` by hand;
  `record_locked` owns the slot for every path, so no caller can rewind
  or double a published counter.
- `lock_poison_tolerant` no longer re-locks the node ledger synchronously
  on the recovery path: a caller that held the node lock while recovering
  a poisoned foreign mutex deadlocked on the ledger it already owned.
  Recovery now ticks `poison_recoveries` best-effort via `try_lock` —
  a skipped tick under-reports one recovery; a synchronous re-lock wedged
  the caller forever. The doc comment now says exactly that.

### Documentation

- Every published number now carries a source. The README benchmark
  tables are marked historical (2026-10-24 / 2026-10-25 full 50-sample
  runs, command included); the contended table quotes the archived
  2026-10-25 artifact (925 / 899 / 146 / 34 ns — the previous 918 / 911 /
  140 / 35.5 values match no artifact) and shows the 2026-10-26 smoke
  re-run alongside, with the rule "quote full-run artifacts, not smoke
  checks". Storm numbers now match the 2026-10-25 reports
  (`wall_ms=53/479/199`, 101,290 lease samples) and name their artifacts.
- README no longer names `pool-gates` (an artifact name) as the CI job
  for the pool storms — it is the `release-ignored` job of `ci.yml`; the
  guard bullet no longer claims a `Copy` shell (`SnubberGuard` is not
  `Copy`); `src/lib.rs` says five layers, matching README/USER_GUIDE.
- `docs/INVARIANTS.md`: "four properties" → thirteen invariants (the
  document lists 13); the `mem::forget` example size is corrected to
  1,648 B with the measurement command (`-Zprint-type-sizes`, nightly
  toolchain log) instead of the unsourced 2,288 B.
- Compile asserts added where the docs claim them: `Priority::COUNT == 3`
  and `DEFAULT_EOF_GRACE_MS == 250` are now `const`-checked
  (`src/policy.rs`, `src/pool/mod.rs`).
- USER_GUIDE storm numbers and contended rows synced to the same
  artifacts; `queue_capacity` semantics documented in one place (async
  waker-queue ceiling, sync admission horizon, async `0` → 1 floor).

### Tests

- New `queue_horizon` test pins the documented async asymmetry of
  `queue_capacity(0)`: the waker-queue capacity floors to 1, exactly one
  waiter parks, the second is refused with `QueueFull` and the full
  policy fires (sync `0` means the horizon fires on every admission —
  different semantics, both now tested).
- New `affordances` test proves `lock_poison_tolerant` recovers a
  poisoned foreign mutex while another thread hammers `stats()`: the
  recovery never re-locks the ledger, `poison_recoveries >= 1` and the
  recovered guard reads 0.
- Test honesty (stage 6): `wakes_happen_after_drop_in_node` is now a
  function-scoped order audit (every `notify_all`/`w.wake()` in node.rs
  follows a `drop(inner)` in the same function) instead of a count
  heuristic; the termination example exports `frames_sum`/`arms_sum`/
  `commits_sum` plus a `misclassified_stillborn` contradiction counter,
  and `supervisor_termination` asserts cross-source floors (`arms_sum >=
  ambush + escape`, `commits_sum >= escape`) instead of tautological
  sums; all feed-path benches assert the parse result inside
  `b.iter_black_box` and `bench_hygiene` fails any `.feed(` line whose
  result is not asserted.
- New `tests/rollback_panic.rs`: panicking rollbacks on explicit abort
  return `SnubberEvent::Aborted`, fire the rollback exactly once and
  release the slot (SnubberGuard, node TrackedGuard, ErasedGuard).
  Negative runs verified per root fix (lib.rs, erased.rs): disabling the
  catcher turns the tests red. The erased probe test was updated from
  pinning the old "abort surfaces the panic" behaviour to the
  documented swallow contract. README badge 416 → 419.
- New `supervisor_pool` tests: attach-during-grace (thread + attach
  handshake, no sleep margin) leaves the successor's `last_exit`/
  `last_class` at `None`; a reaper without a successor still stamps its
  own telemetry. Negative run verified: removing the generation/pid
  guard turns the attach test red. README badge 414 → 416.
- New `tests/queue_horizon.rs`: `TripBreaker` and `RollbackAndAlert`
  armed on `BufferOverflow` do not fire on horizon hits over
  `queue_capacity(0)`; the gauge still exports `buffer_overflow_total`
  and the conservation law holds; the async `QueueFull` refusal with the
  same policy still trips the breaker and books `bulkhead_rejections`.
  Negative run verified: restoring `record_locked` in the horizon branch
  turns 2 of 4 tests red.
- New `tests/metrics_reporting.rs`: every automatic path exports exactly
  one metric sample with `count >= 1`, the hook snapshot carries the same
  alert count, named vs automatic reports of one kind agree on the
  reported slot and the mechanism column, and BufferOverflow records
  never enter the conservation law. Negative run verified: removing the
  centralized `reported[]` bump turns 5 of 6 tests red.
- `trap1_no_double_count_implicit_drop_with_reported` now pins the fixed
  contract: one guard settle produces `reported[ImplicitDrop] == 1`
  (previously it pinned the invisible-fault bug as `== 0`).

### Tooling / CI

- The three stress harnesses share one SKIP verdict formula:
  platform-gated skips are printed and allowed locally, but any skip
  fails the run in CI. `stress_harness.sh` counts `SKIPPED` and gates the
  verdict on it unless `CI` is unset; `stress_multi_lineage.py` gained
  the same `CI`-aware rule; `stress_harness.ps1` prints SKIP rows
  (tri-state OK/FAIL/SKIP) and fails the verdict on any skip. Negative
  runs verified against the real verdict lines with synthetic state.
- The Miri leak-lane comment now references the actual exclusions
  (`tests/channel.rs:13` throttle retry, `tests/task_engine.rs:296`
  wall-clock TTL) and documents that the lane intentionally mirrors the
  core suite set — the concurrency suites rely on real-time deadlines and
  process spawns Miri cannot schedule deterministically.
- README no longer claims the heavy kill lanes gate every push: the
  multi-lineage POSIX storm (4×250 armed-hold + 4×500 free-running) and
  a 25-kill termination smoke gate every push/PR; the 200-kill
  single-process harness, the 100-kill PowerShell harness, the Win32
  multi-lineage storm and the full `#[ignore]` termination gate run on
  the weekly schedule or manual dispatch.
- USER_GUIDE documents all three `queue_limit`/`queue_capacity` semantics
  in one place (async waker-queue ceiling, sync admission horizon,
  telemetry-only behavior, and the async `0` → 1 floor).
- `scripts/release.sh` now parses every `SKIP (...)` line in the compile
  gates log and tolerates exactly one, verbatim —
  `SKIP (cargo-semver-checks not installed)`. Any other SKIP fails the
  release; the old condition only checked the presence of the semver
  SKIP, so a combination like "semver SKIP + Gate 5 SKIP" passed.
- Every ignored release lane (`supervisor_pool_stress*`,
  `stress_breaker_churn`, `supervisor_termination`) now requires
  `executed > 0`: a green `0 passed` (renamed test, cfg'd-out module,
  target-filter typo) fails the gate instead of silently passing. The
  same executed>0 check is mirrored in the `release-ignored` and
  `termination-smoke` CI jobs (bash shell on both Linux and Windows).
  Negative proofs: a synthetic log with a foreign SKIP dies; a
  `0 passed` lane log dies; honest logs pass with the executed count.
- README tests badge 404 → 414 (10 new tests from the telemetry and
  queue-horizon fixes); the badge drift gate caught the difference.
- `scripts/release.sh` (plus the new `justfile`) runs the whole release
  gate in one command: fmt, clippy, full suite + doctests + badge drift,
  compile gates, the `#[ignore]` release lanes, the release identity gate
  and `cargo publish --dry-run`. `--fast` stops after the compile gates
  for a pre-commit check. The gap was manual: the 0.6.9 CI failure came
  from a release-only lane that nothing ran before the release.
- The ignored release lanes (pool storms, breaker churn, termination
  smoke) moved from the Stress workflow into `ci.yml`, so they gate every
  push/PR in the main run; the Stress workflow keeps the scheduled storms.
- Miri jobs pin `nightly-2026-10-09` instead of a floating `@nightly`: the
  0.6.9 Miri failure was a std change in a newer nightly, not a repository
  change, so bumping the pin is now a deliberate commit.
- `just check|release-lanes|release|package-dry|publish` wrap the same
  commands in `scripts/release.sh` for humans and agents.

### Tests

- `block_caller_parks_then_admits_freed_slot` no longer bets that a 10 ms
  sleep leaves the freeing commit inside the 50 ms park window. A runner
  can deschedule the committing thread for longer than that margin, after
  which the parked caller legally reports `BulkheadFull` (the CI failure
  was this, not a lost slot: the old shape passed 900 local runs). The
  friction hook now proves the caller reached the full-bulkhead branch and
  blocks until the holder commits, so the re-check after the park always
  sees the freed slot.

### Docs

- `docs/INVARIANTS.md` gains "Test threshold convention: determinism, not
  percentages": volume floors must be tied to the mechanism under test
  (`admitted >= breaker_trips`), never to a share of attempts, and
  best-effort standard-library behavior must not be asserted without
  `cfg(not(miri))`. A companion section, "Test synchronization convention:
  handshake, not sleep margins", forbids sleeping as cross-thread
  synchronization and requires bounded waits.

## 0.6.9 — 2026-10-25

Eighth-audit batch (hook panics, scanner reset order, lock hygiene, honest
tests and benches, CI teeth). No API removals.

### Fixed

- A panicking `friction_fn` / `pressure_fn` / custom backpressure hook no
  longer leaves `half_open_probe` set forever: the probe flag is cleared
  under a fresh lock and the panic is resumed to the caller. A wedged
  breaker used to survive until a manual `reset()`.
- `SnubScanner::push` resets its frame state before invoking `on_token`,
  so a caught panic inside the callback can no longer redeliver the same
  token on the next chunk.
- `publish_fault` swaps `last_fault` and drops the replaced `FaultRecord`
  after the node lock is released; an owned `NodeName` no longer runs
  foreign destructor code inside the critical section. The doc comment
  now also states the one-time `flight` realloc that follows an external
  shrink.
- Re-parking an async waiter replaces its `Waker` with `mem::replace` and
  drops the old waker outside the node lock; `deregister_waiter` removes
  the exact parked entry and drops it outside the lock as well. A waker
  destructor can no longer re-enter or deadlock the node.
- `Conduit` wakes parked futures via `mem::take` (no allocation under the
  wakers lock) and drops a replaced `ResumeFuture` waker after unlock.
- The node reads the clock outside its mutex: rate-limit admission takes
  the timestamp before locking, and `check_timeout` snapshots the settle
  time after rollback but before the fault record is written.
- `PipeDrain::feed` after `finish()` now increments `feeds_after_eof()`
  and reports zero frames for the discarded chunk; the documented clock
  rule matches the code (no clock read under a drain lock).

### Changed

- Documentation names the real receivers and layers: `lane.attach` /
  `lane.note_death` (not `pool.*`), `PipeDrain::{feed, finish, write_to}`
  as `std` transport on top of the `core` scanner, and `pool.report()` as
  one included `Vec<LaneSnapshot>` allocation rather than "no
  allocation". Public accessors for `PoolConfig`, `PoolLane`,
  `PoolReport`, `ExecutionEnvelope`, `ProfileHarness`, `HarnessReport`
  and `ProcessToken::commit` are now listed.
- README bench IDs match the source (`*_threads_6`, `push_pop`,
  `classify_stall_ceiling`), and the latency table carries a measurement
  date, the Criterion command, and the CI artifact lane
  (`bench-criterion-<run_id>`, `scripts/bench.sh --skip-run`).
- Ordering comments now match the code: the gateway depth gauge is
  Relaxed (the authoritative cap is `try_send`; the throttle latch stays
  Release/Acquire), the two free-standing `SeqCst` fences in the erased
  transaction are gone (the ledger mutex is the ordering edge),
  `next_sequence` explains why a Relaxed ticket is enough, and the lane
  `pid`/`drain` pair documents the absence of a panic point between the
  two stores. `task_ttl` is documented as starting at the first park and
  never re-stamping.

### Tests

- New coverage: a winner dropped between settle-pop and re-poll hands the
  free slot to the next waiter; a HalfOpen async probe cancelled in the
  queue does not wedge the breaker; an async admission recovers from a
  deliberately poisoned ledger (`poison_recoveries`, commit,
  conservation). The ISR example now asserts the laser is off after the
  cycle-3 abort as well.
- `stress_breaker_churn` no longer demands that 10% of the storm's attempts
  be admitted. Once the window trips, HalfOpen admits exactly one probe and
  refuses every other attempt, so the lane structurally yields about four
  admissions per trip (release: 301 admitted, 75 trips, 11698 breaker
  refusals). The volume floor is now tied to the churn the lane exists to
  prove (`admitted >= breaker_trips`, `breaker_rejections > 0`) instead of
  to a share of attempts that only held while the storm ran slowly enough
  to admit work before the first trip.
- `fake_waker_identity_matches_allocation` keeps its clone-identity
  assertion only outside Miri. std builds the `Arc` waker and its clone
  vtable as two identical const promotions and relies on LLVM merging
  them, so the clone carries the same vtable pointer natively; Miri never
  runs LLVM and each promotion stays a separate allocation, making
  `will_wake` (best-effort by contract) return false. The engine dedups on
  its own `Arc<AtomicBool>` identity, so no library behavior depended on
  the assertion.

### Tooling / CI

- Both gate flavors print an identical `PARITY:` fingerprint, and a
  Windows CI step fails if the `.sh` and `.ps1` counters diverge; CI
  detection is strict (`CI=true|1` only) in bash and PowerShell.
- New release-identity gate compares `Cargo.toml`, the latest tag and
  `CHANGELOG.md` (warns for a documented-but-untagged version, fails for
  an undocumented one; never creates tags).
- New `miri-leaks` job runs without `-Zmiri-ignore-leaks` over the lib
  and 14 suites; the scheduled `bench-full` job archives Criterion
  output; `termination-smoke` exercises the supervisor kill path on
  Ubuntu and Windows on every push.
- Stress harnesses require `--launch` or an explicit
  `--external-supervisor` under CI; the supervisor test passes the
  latter because it owns the child itself.

## 0.6.8 — 2026-10-23

Seventh-audit batch (core accounting, async liveness, erased slots, honest
examples and gate teeth). Details: USER_GUIDE "Fixed in 0.6.8".

- Conservation is now an API-level invariant:
  `admission_total() == total_started + shed_total()`. `shed_total()`
  counts every first-pass refusal exactly once (capacity, stale, rate,
  breaker, poisoned lock, memory); `evicted` and `preempted` are
  side-events of an admission already in `total_started`, so they no
  longer inflate the shed side. `rate_rejected` is no longer mirrored
  into `bulkhead_rejections`.
- Depth gauge subtracts `explicit_aborts` and `implicit_drops`, so a
  settled transaction never shows as in flight.
- Rate shedding has its own `FaultKind::RateLimited` (index 12,
  `FaultKind::COUNT` 13): a `BulkheadFull` TripBreaker override can no
  longer fire on a paced refusal, hooks and health see the honest event,
  and `rate_rejected` is booked once. `SnapshotV1` bumps to v5 and grows
  the reported tail 12 → 13 (322/508 bytes).
- Admission stamps and fault details are built in PHASE B: every
  rejection arm drops the state lock before `arming.clone()`/`format!`,
  and the rate counter uses `saturating_add` like every other bump.
- Async admission probe state is cleared on every terminal `Ready`
  (spurious re-polls no longer leave `half_open_probe` wedged, and a
  Ready future retires any queue entry it still owns by identity).
  `Drop for EnterFuture` hands the freed slot to the next waiter in
  `deregister_waiter`, so a cancelled task cannot strand a quiescent
  queue; an evictor's verdict clears the flag without waking a phantom.
- `enter_erased` / `enter_supervised_erased` book the success when the
  erased guard's `commit()` runs, never at admission: `committed += 1`,
  one window outcome, streak reset; a guard that never commits cannot
  inflate `success_rate_per_mille`. Rollback books one planned abort
  (`explicit_aborts += 1`, keeping the depth gauge balanced against the
  `total_started` booked at admission) and still re-opens a HalfOpen
  probe.
- Poison recovery: gateway books, conduit drafts and the cancel-reason
  guard recover `into_inner` instead of silently dropping the update
  (`.ok()` issued the notify unlocked, re-opening a lost-wakeup window).
- `Protect` runs the rollback BEFORE taking the resource (same order as
  `Drop`): the closure can still read the protected value.
- `PipeDrain::feed` takes the ingest lock before the EOF check (a
  concurrent close is fully ordered, not half-applied), and the
  injectable clock is read once per chunk OUTSIDE the state lock
  (`apply_token_at`).
- Channel: HWM is computed as `c - c/4` (no overflow at absurd
  capacities) and the throttle depth is loaded with `Acquire` so a
  refusal cannot act on a stale depth.
- Overflow and clamp fixes: `task_ttl` converts `as_millis` with
  `try_from` (saturates, never wraps), restart-window reserve uses
  `saturating_add`, lane histograms are clamped to the aggregate
  `in_flight` after lock recovery, and profile stage slices are clamped
  to `stage_count`.
- Tests, examples, benches: conservation asserted in the invariants,
  bulkhead-ceiling and industrial suites; the pool storms gate app-side
  token receipt and lane lineage (`tokens_seen`, `token_lane_mismatch`),
  so `rescued == ambush` is no longer the only witness; rollback effects
  are observable
  state (DB handles, iron energization, laser latch, real advised sleep)
  instead of counters that can be faked; fake wakers use
  `Waker::from(Arc<dyn Wake>)` (Miri strict provenance); the bit-flip
  fuzzer asserts a non-zero, fully accounted mutant sweep; bench hygiene
  covers `TaskConduit::pair` / `PipeDrain::drain_sync`, rejects
  `std::io::sink()` (a discarding writer understates `write_to`), and
  requires `iter_batched` for per-sample drains so construction stays
  untimed.
- Scripts/CI: a grep error (rc=2), a missing report, a skipped check
  (missing tool or thumb target) and a baseline equal to `HEAD` all FAIL;
  `--locked` on every cargo invocation (thumb gates, publish dry-run);
  `cargo-semver-checks` exposes no `--locked`, so both API gates hash
  `Cargo.lock` around the run and FAIL if it was rewritten; the stress
  harness refuses to publish a verdict when its stats report is missing
  or empty; harnesses fail on zero samples; deadlock literals removed
  from termination reports.
- Settle paths no longer call `mem::forget`: `SnubberGuard` and
  `ErasedGuard` `commit`/`abort` construct their events and let the
  disarmed guard drop normally. The fields were taken first, so the
  forget was harmless, but it skips drop glue for every field — the
  exact shape that leaked one `NodeInner` per transaction from
  `TrackedGuard` before 0.5.2. `#![deny(clippy::mem_forget)]`, a new
  script gate (7/7) and a 250k-guard allocation storm (Layer 1 guards
  owning heap payloads plus the node erased path in commit and abort) keep
  it removed.
- One allocation per erased admission: `ErasedGuard` now holds a single
  boxed `ErasedSettle` transaction instead of separate rollback and commit
  closures, and the node path boxes a transaction that already carries its
  node clone. `enter_erased_auto + commit` measured 301.6 → 207.75 ns
  (two filtered Criterion runs: 209.21 / 207.75 ns mean), with none of the
  settle, abort-bookkeeping or supervisor-token semantics changed.
- Miri runs with `-Zmiri-strict-provenance` in both CI lanes.
- The benchmark exporter publishes only `.../new/estimates.json` (a real
  ns latency): it also published `change/` entries, i.e. a relative delta
  printed as "nanoseconds". CI now runs one bench and greps the exported
  table instead of only compiling benches. The ignored pool storms and
  `stress_breaker_churn` run on every push and PR (`pool-gates`); only
  the supervisor and multi-lineage harnesses stay scheduled.
- Docs: README saturation claim is measured (~1.1M admissions/sec on six
  contending threads), `WorkerPulse` footprint is arch-conditional,
  pool numbers re-measured (attach/reap ~192/~173 ns with the drain built
  outside the timed window, `write_to` ~2.1 ns into a real writer, report
  ~111 ns, lease sweep ~517 ns), the drain and async rows re-measured
  (`PipeDrain::feed` 21.2 → 10.8 us per 256 frames, async ready poll
  ~152 ns, `enter_erased_auto` ~208 ns after the single-box transaction
  replaced two boxes and two `Arc` clones), storm timings re-measured, and the API table
  gains `PoolReap` / `LaneSnapshot` / `PoolTotals` / `expired_lanes` /
  `DEFAULT_EOF_GRACE_MS` / argv-twin rows.
- The POSIX storm harness gates descriptor growth on macOS too: it counts
  `/proc/<pid>/fd` on Linux and parses `lsof -nP -p <pid> -F f` on macOS
  (1 Hz, since `lsof` walks every descriptor), so the four per-lane handle
  checks no longer degrade to platform skips. A skipped check still fails
  the verdict, so the macOS lane is judged on executed checks.
- Two timing-sensitive tests no longer race the clock:
  `spurious_repolls_do_not_reset_the_ttl_clock` ignores Miri and stops
  predicting the poll result from wall-clock time (it accepts either park
  or expiry at the 1 ms-clock boundary and asserts `queue_wait_ms` spans
  from the first park), and `spawn_guarded_binds_thread_guard_leash`
  synchronises through an mpsc handshake instead of a 20 ms sleep.

## 0.6.7 — 2026-10-23

Sixth-audit batch: conduit waker identity, core-exposed drain, honest
bookkeeping, CI/script teeth. Details: USER_GUIDE "Fixed in 0.6.7".

- `ResumeFuture` waker identity is an explicit `Arc<AtomicBool>` flag
  (`Arc::ptr_eq`), not `will_wake`: two futures polled by one task no
  longer cross-match and orphan a sibling's directive-queue entry. Wakers
  are drained from the queue under the lock and woken OUTSIDE it —
  executor code can never run under the conduit's wakers lock.
- The conduit's wakers lock is poison-tolerant (`into_inner` recovery);
  the pause/cancel directive path survives a foreign panic under the
  lock.
- Async queue TTL origin is preserved across spurious re-polls: a
  re-parked waiter keeps its FIRST-park stamp, so a re-poll loop can
  never starve `task_ttl` (regression test:
  `tests/task_engine.rs::spurious_repolls_do_not_reset_the_ttl_clock`).
- `memory_pressure` verdict loads pair with the `Release` store
  (`Acquire`): a pressure refusal can no longer be decided from a stale
  verdict.
- New `drain` module in core: `ProcessToken` / `SnubScanner` /
  `OP_*` / `TOKEN_*` are available without `std` (feed-driven parsing
  on bare metal); `supervisor` re-exports keep every historical path.
- A snapshot const-assert pins the flag region below the CRC region:
  adding `FaultKind`s beyond the wire contract is a compile error, not
  a silent collision.
- Honest refusal buckets in four suites: swallowed `Err(_)` arms are
  gone; every attempt lands in exactly one counted bucket tied to the
  node's own books (`stress_breaker_churn`, `priority_preemption`,
  `transition_orphan`, `industrial`, `bulkhead_capacity`).
- CI: `--locked` on every cargo invocation; a semver-gate SKIP fails
  under CI (`check_api_diff` both flavors); `bench.sh` refuses an empty
  snapshot in every mode and publishes atomically (temp + move).
- Storm harnesses: zero sampling fails the leak gates instead of
  silently skipping them; `stress_multi_lineage.py` reports
  platform-skipped checks as explicit SKIP, not PASS.
- Docs: `failure_rate` documented as a windowed-batch breaker (the
  counters reset after every `window` outcomes — not a continuous
  sliding window); README `no_std`/`alloc` surface list matches the
  module gates; USER_GUIDE cites the TTL regression test and adds the
  `run_supervised_guard_with_grace` / core `drain` API rows.

## 0.6.6 — 2026-10-23

Fifth-audit batch: async liveness, storm-harness hygiene, CI hardening.
Details: USER_GUIDE "Fixed in 0.6.6".

- `ResumeFuture` implements `Drop`: the parked waker is removed from the
  directive queue when the future is dropped mid-`.await` (no leak, no
  stale wake).
- `enter_async_auto` zero-wait re-check no longer refuses a verified-free
  slot: after the post-park admission re-check, a `wait_ms == 0` caller
  (AdviseOnly / Custom pacing) is admitted like any other.
- Async waiter identity is the queue entry itself (own `evicted` flag
  via `Arc::ptr_eq`), not `will_wake` — two `EnterFuture`s inside one
  task no longer cross-match and orphan a sibling's queue entry.
- Async queue TTL now counts from the FIRST park (re-parks refresh the
  waker, not the clock), so queue-wait telemetry and TTL expiry measure
  real waiting time.
- `note_memory_kb` stores the pressure verdict under the bulkhead lock
  (no more out-of-lock window where pressure state could be overwritten
  by a concurrent admission).
- `TaskConduit::update_state` recovers from a poisoned state lock
  instead of silently no-op-ing the update.
- `run_supervised_guard_with_grace`: EOF grace is configurable; the
  pool's `note_death` grace loop backs off with a poll-count bound so a
  non-advancing injected clock cannot spin forever.
- OP_STATE doc corrected to 3 frames; the false-"SNUB"-byte desync
  limitation is documented on `PipeDrain` (wire-format break to fix in a
  major release, not silently).
- Storm harnesses: `stress_multi_lineage.py` kills the lane worker after
  a hard-killed supervisor (no orphaned spinner past `finalized`),
  `stress_harness.sh` traps EXIT/INT/TERM and uses `mktemp` latency
  files; `bench.ps1` refuses to publish an empty snapshot and drops its
  dead `$layer` table; `bench.sh`/`bench.ps1` units unified (`us`).
- Gate scripts: PowerShell flavor is case-sensitive on parity-critical
  gates; the `.sh` Gate 6 no longer swallows the baseline==HEAD warn.
- CI: `permissions: contents: read` on both workflows, `concurrency`
  group + `timeout-minutes` on every CI job, `--locked` on every cargo
  invocation, checkout/upload-artifact re-pinned to current majors
  (v7), and a scheduled/manual Windows lane running the Win32
  `stress_multi_lineage.ps1` storm.
- Tests: `failure_rate` clamp/disable semantics covered (3 new tests),
  `enter_paced_async` both branches (AdviseOnly refusal + admit after
  pace), `print_health` smoke, exact-JSON-number parsing for storm gates
  (`json_num`), token-bucket test de-flaked, storm gate now an
  `#[ignore]` breaker-churn stress; the three numeric storm-gate asserts
  no longer match by substring.
- Docs: erased-guard "iron law" scope correction (the admission probe is
  not a lifetime lease; serialize live workers yourself), wake-window
  semantics in Layer 3, honest `no_std` surface list in the README,
  `PulseTick` reworded as internal, Layer 5 helper exports row.

## 0.6.5 — 2026-10-23

Fourth-audit cleanup batch: every remaining P2/P3 finding closed.

- `OverflowPolicy::EvictOldest` / settle paths / `enter_erased` bulkhead
  semantics: previously implicit behavior is now documented where the
  contract lives (async settle wake window; erased guards are not bounded
  by `max_in_flight` once admitted — enforce an in-flight ceiling in the
  daemon).
- `failure_rate(window, 0)` no longer trips on a fully successful window:
  `percent` is clamped to `1..=100` (`0` reads as "any failure in the
  window trips", not "trip always").
- `PipeDrain::take_armed` / `take_state`: a poisoned lock is recovered
  instead of silently dropping a rescue token (`DrainState` holds no
  tearable invariants).
- `next_sequence` is a pure atomic bump (the bulkhead lock is no longer
  taken to mint a ticket).
- Gate-script hygiene: portable POSIX grep patterns (no `\b` / `\s` /
  `[[:<:]]`), indent-safe `record_locked` extraction, BOM-safe lib.rs
  reads, honest SKIP summary with a skip count, SKIP = FAIL under CI.
- CI: `cargo doc` runs with `RUSTDOCFLAGS=-D warnings`; `Cargo.lock`
  freshness enforced via `--locked`; the PowerShell gate flavor runs on
  the Windows job (with the bare-metal target and `cargo-semver-checks`
  installed there); weekly scheduled re-runs of both workflows; GitHub
  Actions pinned to commit SHAs.
- New tests: `take_stdin_digest` full/torn stdin round-trip, std-side
  contract tests for the firmware-facing `CpuClock` / `IdlePark` /
  `WdtSink` family, a strengthened Condvar-pause regression test.
- Docs: unmeasured cost rows are explicitly marked `structural:`; public
  `CHANGELOG.md` added; API quick-reference rows for `SafeLockExt`,
  `ClosurePolicy`, `TransactionHeader`, `GatewayStats`, `ResumeFuture`,
  `PulseTick`, `LeaseEvent`.

## 0.6.4 — 2026-10-23

Post-release audit strike: one core-concurrency hole and a decorative CI
gate closed with teeth. Details: USER_GUIDE "Fixed in 0.6.4".

- `EvictOldest` overbooking capped at exactly one slot (ceiling `max + 1`);
  beyond it callers are refused with `BulkheadFull`. Before: repeated
  re-booking desynchronized `lane_in_flight` from `in_flight` and
  permanently disabled Critical-over-Background preemption.
- `enter_erased` / `enter_supervised_erased`: the half-open probe settles
  before a panicking rollback resumes — the breaker can no longer weld
  shut.
- Async `EvictOldest` reaches full sync-arm telemetry parity
  (`record_locked` + fault publication + per-kind policy).
- Gate 6 (public API semver) runs for real in CI: full-history checkout
  with tags, `cargo-semver-checks` installed, tag `v0.6.3` as the
  baseline; a SKIP under CI is a failure.
- `stress_harness.sh` kills only identity-verified worker processes
  (image name + monotonic start time).

## 0.6.3 — 2026-10-23

Audit closeout: every finding fixed in one release. Details: USER_GUIDE
"Fixed in 0.6.3".

- `OP_SNAPSHOT` frames are neutral (arm nothing, clear nothing).
- Catastrophic `report()` calls funnel through the same streak trip as
  the guard settle paths.
- Ghost async waiter eliminated: a future admitted on a spurious re-poll
  deregisters its queue entry unconditionally.
- `take_state` consumes the digest; `last_state` is the non-consuming
  peek.
- Generation-0 children get `Stdio::null()` stdin.
- `admission_total` parity between `SnapshotV1` and `SnubberStats` (both
  exclude `evicted`).
- Harness/gate hardening: launch-without-report = FAIL, Gate 3
  empty-extraction = FAIL, line-anchored attribute greps, wedged
  supervisors are reaped and failed.

## 0.6.2 — 2026-10-23

Audit hardening: 13 behavioral fixes. Details: USER_GUIDE "Fixed in 0.6.2".

- Default `BulkheadFull` action is `DiscardSilently` (capacity shedding
  never trips the breaker); an explicit per-kind `BreakerOpen` still sheds
  immediately.
- The HalfOpen probe settles on the erased guard's real outcome.
- `IsrBridge::try_pop` drains malformed cells instead of delivering them.
- One probe at a time in HalfOpen.
- Watchdog timeouts feed the streak breaker.
- A `BulkheadFull` rejection no longer burns the caller's epoch.
- Custom friction advisories are clamped to the node's pacing ceiling.
- Supervisor EOF wait is a bounded 250 ms grace, not a busy spin.

## 0.6.1 — 2026-10-23

Concurrency liveness: six fixes, each with a deterministic regression
test. Details: USER_GUIDE "Fixed in 0.6.1".

- Gateway depth drift fixed (signed counter; the throttle latch can no
  longer wedge on an empty queue).
- A timed-out HalfOpen probe re-opens the breaker instead of wedging it.
- `EpochGate` adopts a new generation at epoch 0 (reboot re-handshake).
- `EvictOldest` wakes the evicted async waiter (its poll resolves
  `QueueFull`, never a zombie park).
- A future dropped while parked deregisters its own queue entry.
- Lane generation bumps before the EOF grace; conduit wakeups are issued
  under the waiter's mutex.

## 0.6.0 — 2026-10-23

Worker pool + true `no_std` + API extensibility.

- `pool`: N isolated worker lineages per process — lanes with attach /
  `note_death` rescue semantics, lease sweep, eviction, per-class
  counters, multi-lineage storm harness.
- True `no_std` (`core` layer builds for `thumbv7em-none-eabihf`),
  `PipeDrain::feed` / `ProcessToken::write_to` drain-feed transport,
  injectable clocks (`MonotonicMs`, `ClockFn`).
- Extensibility: `#[non_exhaustive]` public enums, `Arc` friction /
  pressure handlers, `snub_check_map!`, dynamic `NodeName`, supervisor
  argv API, format-ceiling contract.

## 0.5.3 — 2026-10-22

Benchmark coverage + re-measurement of the whole public surface; POSIX
storm harness and CI wiring.

## 0.5.2 — 2026-10-22

Settlement `Arc` leak eliminated with zero-leak gates; multi-lineage
storm with honest kill classification.

## 0.5.1 — 2026-10-19 / 2026-10-20

Three-circuit hang classifier (`CpuSpin` / `SleepBlocked` /
`DeadlockStall`) with the `DeadlockSuspected` settle verdict;
builder-sealed pass-through canary with a differential proof.

## 0.5.0 — 2026-10-18

Priority preemption, supervisor restart policies, invariant hardening.
