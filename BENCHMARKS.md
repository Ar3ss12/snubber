# Benchmarks

The single home for measured latency figures in the public docs. This file
carries the full operation matrix, the contended group and the API & cost
quick reference; README.md and docs/USER_GUIDE.md link here and quote no
numbers. `tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact`
checks every `~<number>` citation below against the committed snapshot
[`docs/evidence/2026-10-26_bench_snapshot.md`](docs/evidence/2026-10-26_bench_snapshot.md)
— the docs never carry a second copy of a measurement.

Measured on AMD Ryzen 5 4600H release build
(`cargo bench --bench admission_bench`, Criterion: 800 ms warmup,
2 s measurement, 50 samples, isolated node per bench). Values are the
second of two full runs on 2026-10-26; the first run agreed within
run-to-run noise outside the contended group, which moves by design.
The re-run exists because the 2026-10-24 table was not reproducible: a
full run at that commit today matched every lock-free row and read
30–40% higher on the guarded admission rows, which exposed the clock
regression described under Provenance.

**Provenance.** Both tables are snapshots from full 50-sample runs on
the listed machine — they are not re-derived on every release and move
with hardware, compiler and load. The 2026-10-26 re-run caught a real
regression: `7481e9a` had hoisted an unconditional `now_ms()` out of
the node mutex, so every sync admission paid an OS clock read even when
the token bucket was disarmed (the default). Bench: `node.enter +
commit` 126 → 166 ns; the bucket's arming is now mirrored outside the
mutex and the stamp is taken only when a token is actually checked,
restoring 124 ns. The 2026-10-27 pass went further: a guard with no
deadline armed takes no stamp at all (`needs_stamp`), which also removed
the clock read from the passthrough canary — `node.enter + commit`
124 → ~91 ns, `node_passthrough_enter + commit` 96.5 → ~55 ns. `cargo bench` regenerates the whole table into
`target/criterion/*/new/estimates.json`. The scheduled `bench-full` CI
lane runs `cargo bench --locked --bench admission_bench`, then
`bash scripts/bench.sh --skip-run --github` to render
`target/bench/snapshot.md` — generated output inside the gitignored
`target/` tree, not a file in this repository — and archives that file
plus the raw Criterion trees as artifact `bench-criterion-<run_id>`. To re-print the table from an
existing `target/criterion` without running benches again, use
`bash scripts/bench.sh --skip-run`. Both exporters (`bench.sh`,
`bench.ps1`) carry the same 52-entry label map, so every published row
reads as an operation rather than a Criterion key. Differences below
~2 ns are inside run-to-run noise and are not claimed as wins.

## Full operation matrix

| Operation | Architecture Layer | Average Latency | Notes |
|---|---|---|---|
| `SnubberGuard::new` + `commit` | Layer 1 (`core`) | **~5.4 ns** | Stack move + owned-field drop, zero heap allocation |
| `EpochGate::observe` | Layer 1 (`core`) | **~2.6 ns** | 2-D generation + monotonic epoch filter |
| `classify_stall_ceiling` (bench; `classify_stall` is the 4-arg wrapper that delegates here) | Layer 1 (`core`) | **~2.0 ns** (const math) | `CpuSpin` / `SleepBlocked` / `DeadlockStall` from one sample pair (the settle verdicts `SpinDetected` / `DeadlockSuspected` come from `guard.check_timeout`) |
| `WorkerPulse::kick` + `PulseWatch::observe` | Layer 1.5 (`core`) | **~5.8 ns** | One-word heartbeat store + observe fold; no lock, no allocation |
| `IsrBridge::try_push` + `try_pop` | Layer 1.5 (`core`) | **~7.1 ns** | 80 B SPSC ring, lock-free; malformed cells drained on pop |
| `IsrBridge::trip` + `take_trips` | Layer 1.5 (`core`) | **~9.3 ns** | 32-bit latch swap; trip bypasses a full ring |
| `node.enter + commit` | Layer 2 (`std`) | **~91 ns** | Full bulkhead, circuit breaker, auto-commit; no clock read when neither the token bucket nor a deadline is armed (unstamped guard) |
| `node.enter_auto + commit` | Layer 2 (`std`) | **~93 ns** | + atomic sequence minting |
| `node.enter_with_priority(Critical)` | Layer 2 (`std`) | **~87 ns** | Priority lane; same band as plain enter (one measurement) |
| `node.transition` | Layer 2 (`std`) | **~90 ns** | ONE-enum state machine, zero flag drift |
| `node.transition_checked` + `LegalMove` | Layer 2 (`std`) | same path, no extra work | Compile-time only: the `LegalMove` proof erases to the same `transition` call; illegal moves fail at compile (E0277) |
| `enter_erased_auto` | Layer 2 (`alloc`) | **~205 ns** | ONE boxed transaction (rollback + commit hook + node clone in a single allocation; 0.6.8 moved the breaker outcome onto the guard's real `commit`). 301.6 → ~205 ns after folding two boxes and two `Arc` clones into one |
| `node_passthrough_enter + commit` | Layer 2 (`std` canary) | **~55 ns** | Open-doors transit: lock-free count + unstamped dummy guard, no slots/breaker/`EpochGate`/clock |
| `node_passthrough_scoped + commit` | Layer 2 (`std` canary) | **~51 ns** | Same transit through `scoped()` — work runs, books stay clean |
| `ConflateSender::publish + take_latest` | Layer 2 (`std`) | **~21.7 ns** | One-slot latest wins, no allocation, no `Clone` bound |
| `ConflateSender::publish` (occupied slot) | Layer 2 (`std`) | **~15.7 ns** | Replaced value counted, never queued |
| `guard.commit_enveloped(64)` | Layer 2 (`std`) | **~142 ns** | Payload corridor exact; wall corridor only for profiled guards |
| `TaskConduit::check_directive` | Layer 2 (`std`) | **~1.0 ns** | One atomic load per poll, no lock |
| `gateway send -> recv roundtrip` | Layer 2 (`std`) | **~50.8 ns** | Bounded MPSC send plus receiver wake |
| `gateway send` (HWM/LWM throttled) | Layer 2 (`std`) | **~16.8 ns** | Latch fast path while inside the high/low horizon |
| `node.next_sequence()` | Layer 2 (`std`) | **~4.8 ns** | One atomic `fetch_add` ticket, shared across clones |
| `node.stats()` snapshot | Layer 2 (`std`) | **~58.4 ns** | One short lock, fold into `SnubberStats` |
| `enter_async_auto` | Layer 3 (`std`) | **0% CPU** | Pure `core::task::Waker` parking, no runtime deps |
| `enter_async_auto_profiled` | Layer 3 (`std`, opt-in) | structural: 1 `Box` + 2 clock reads | Same waker parking, profile arms on the admitting poll |
| `enter_async_auto` (ready poll) | Layer 3 (`std`) | **~156 ns** | Full admission through the future; the ready path allocates nothing |
| `enter_async_auto` park/wake/admit roundtrip | Layer 3 (`std`) | **~542 ns** | Same-thread park (queue push + identity flag) -> slot release wake -> admitting re-poll; no executor, not a cross-core roundtrip |
| `static_node.enter + commit` | Layer 2 (`std`) | **~125 ns** | Compile-time capacity assert + bulkhead + auto-commit |
| `node.note_pulse_stall(lag_ms)` | Layer 2 (`std`) | structural: 1 atomic max | Missed heartbeat lands in `SnapshotV1` (pulse fields since v3; frame is v5) + `AnomalyKind::PulseStall` |
| `node.note_memory_kb(used)` | Layer 2 (`std`, cold) | structural: 1 lock + 1 atomic store | External memory sample → soft/hard verdict; admissions refuse lock-free above soft |
| `process_token.to_bytes()` | Layer 4 (`core`) | **~7.9 ns** | 44-byte framed wire serialization (`to_le_bytes`) |
| `ProcessToken::from_bytes()` | Layer 4 (`core`) | **~7.4 ns** | 40-byte body parse, no allocation |
| `state_digest.crc()` | Layer 4 (`core`) | **~266 ns** | 64-byte hardware cache line digest verification |
| `PipeDrain::drain_sync` | Layer 4 (`std`) | **~44.5 ns/frame** | 256 frames: 11.4 µs, 22.4M frames/s; the per-sample `PipeDrain` construction is included, so this is a pipeline cost, not a steady-state frame cost (see `feed` below) |
| `PipeDrain::feed` (one 44 B frame) | Layer 4 (`std`) | **~98 ns** | Caller-pushed chunk: scanner + lock + dispatch, no reader thread |
| `PipeDrain::feed` (256 frames/chunk) | Layer 4 (`std`) | **~10.7 µs** (42.0 ns/frame) | Same scanner: one ingest lock per chunk + one short state lock per token (0.6.8 took the clock read outside the lock) |
| `ProcessToken::write_to(&mut sink)` | Layer 4 (`std`) | **~1.8 ns** | 44 B `write_all` into any real `Write` (copy included): no env, no stdout lock |
| `StateDigest::write_to(&mut sink)` | Layer 4 (`std`) | **~615 ns** | Three `OP_STATE` frames + CRC32 seals |
| `namespace_task` pack / unpack | Layer 4 (`std`) | **~1.0 / 1.0 ns** | `task = lane << 48 \| local`; two shifts + mask, no branch |
| `lane.attach` + `note_death` (ambush) → `PoolReap` | Layer 4 (`std`) | **~225 ns** | Full rescue path: drain EOF, token take, generation bump, restart math, per-class counters (drain built in setup; the shared idle lane is re-attached each sample) |
| `lane.attach` + `note_death` (escape) → `PoolReap` | Layer 4 (`std`) | **~187 ns** | Same path when the child settled (commit parsed, no token) |
| `pool.report()` (4 lanes) | Layer 4 (`std`) | **~107 ns** | Rows (`LaneSnapshot`) + `PoolTotals::of`: four short lane locks, totals fold, one `Vec<LaneSnapshot>` allocation (`lanes.len()` rows) included in the number |
| `LaneSnapshot` / `PoolTotals` | Layer 4 (`std`) | `Clone` row / `Copy` totals | `conserved()` = `ambush+escape+stillborn+deadlocks == reaps`; no guessed fifth bucket |
| `run_supervised_guard_with_grace(..)` | Layer 4 (`std`) | one reaped child process | Supervised guard with a configurable terminal EOF grace for slow-flushing children |
| `DEFAULT_EOF_GRACE_MS` (250 ms) | Layer 4 (`std`) | published const | Default terminal EOF grace; `run_supervised_guard_with_grace` and the pool builder override it |
| `lane.note_memory_kb` | Layer 4 (`std`, cold) | **~12 ns** | Per-lane watch: one lock + integer compares |
| `pool.expired_lanes(lease)` (4 lanes) | Layer 4 (`std`, cold) | **~5.4 ns** | One sweep over four silent pipes: one state lock per lane, no child touched |
| `run_supervised_guard_with_memory` | Layer 4 (`std`) | structural: 1 sampler call / 10 ms tick | Soft logs `MEMORY_DRAIN`; hard `kill()` without draining (`DeathCause::MemoryLimit`) |
| `enter_auto_profiled` + `checkpoint_timed` + `commit_profiled` | Layer 2 (`std`, opt-in) | structural: 1 `Box` + 1 clock/stage | Stage wall/CPU breakdown; default `enter*` reads no profile clock |
| `snapshot_v1.encode()` | Layer 5 (`core`) | **~1.11 µs** | 512-byte zero-allocation snapshot CRC32 codec |
| `friction_fn` handler call (`Arc<dyn Fn>`) | Layer 2 (`std`) | **~1.8 ns** | Capture-capable friction: one dynamic dispatch outside the node lock |
| `friction_wait_ms` (adaptive, Arc handler) | Layer 2 (`std`) | **~21.7 ns** | Lock + `Arc` clone + handler; identical to the legacy fn-pointer path (~21.8 ns) |
| `NodeName::Static` clone | Layer 2 (`alloc`) | **~1.5 ns** | Fat-pointer copy, no allocation |
| `NodeName::Owned` clone | Layer 2 (`alloc`) | **~10.0 ns** | One `Arc<str>` increment; `as_str` ~1.0 ns |
| `snapshot_v1()` + `find_anomalies()` | Layer 5 (cold) | structural: 512 B, CRC32 | Zero-allocation telemetry; `assert_no_anomalies!()` CI gate |
| `before.diff(&after)` + `write_triage` | Layer 5 (cold) | structural: pure `Copy` math + `fmt::Write` | Counter deltas since last sample; one ASCII verdict card |
| `memory_limits(soft, hard)` + admission | Layer 2 (`std`) | structural: 1 relaxed load | Over soft → `EnterError::MemoryPressure` without taking the mutex |

**Why the shape, not just the numbers.** The User Guide argues the four
ratios this table depends on: the per-layer split is a
[level budget, not a time budget](docs/USER_GUIDE.md#46c-checkpoint-physics-64-b-is-a-ticket-not-storage)
(`hard - soft` is allocator headroom, not a countdown); the 512 B snapshot
is a
[deliberate lossy checkpoint](docs/USER_GUIDE.md#why-a-naive-serialization-dies-50-mb-state)
because cloning 50 MB of state crosses the hard limit before the first
`write_all`; a pre-fork RSS number is not a container number because
[cgroup v2 counts page cache](docs/USER_GUIDE.md#cgroup-v2-page-cache-counts-against-the-limit);
and `EvictOldest` admits exactly one transient
[`max + 1`](docs/USER_GUIDE.md#fixed-in-064-post-release-audit-strike)
slot — beyond that it sheds like every other caller.

## Contended Multi-Threaded Latencies (Real Thread Saturation)
Measured with half of available hardware cores active (6 concurrent threads under contention, AMD Ryzen 5 4600H), full 50-sample runs of 2026-10-26 (`cargo bench --locked --bench admission_bench contended_multithreaded`; archived as `target/bench/snapshot.md` — generated by `scripts/bench.sh`, not a repository file — together with the raw Criterion trees in the CI `bench-criterion` artifact):

| Contended Operation | Concurrency | Measured Latency | Physical Phenomenon |
|---|---|---|---|
| `contended_node_enter_threads_6` | 6 threads | **~971 ns** | Mutex contention: cache-line transfer between threads |
| `contended_node_scoped_threads_6` | 6 threads | **~923 ns** | Full transactional cycle under contention (entry + work + auto-commit) |
| `contended_gateway_mpsc_threads_6` | 6 threads | **~184 ns** | Bounded multi-producer channel write under parallel thread load |
| `cross_thread_isr_bridge_push_pop` | 2 threads | **~40 ns** | Lock-free SPSC ring: cache-line handoff between threads |

The two full 50-sample runs of 2026-10-26 read 924–971 / 922–923 / 184–204 / 39–40 ns for the same four rows — the contended group moves between runs by design; quote full-run artifacts, not smoke checks. A saturated node admits **~1.1M admissions/sec** under 6-thread contention (uncontended admission is several times faster).

---

## API & Cost Quick Reference

Latency figures are Criterion means from `target/criterion/*/new/estimates.json`
on an AMD Ryzen 5 4600H (`cargo bench --bench admission_bench`). All rows
re-measured 2026-10-26 (full 50-sample run, same run as the matrix above).
Differences below ~2 ns are inside run-to-run noise and are not claimed as wins.

The `Layer` column is the chapter number of [docs/USER_GUIDE.md](docs/USER_GUIDE.md) plus what the row requires: `core` (no `std`), `alloc`, `std`, `cold` (diagnostics cold path), `contended` (saturated bench lane). A row must never disagree with the chapter map in that guide.

| Operation | Layer | Latency / Footprint | When to Use |
|---|---|---|---|
| `snub_check!(cond, reason)` | 1 / core | ~0 ns (single branch) | Universal invariant check, returns `Err` |
| `SnubberGuard::new` | 1 / core | **~5.4 ns** (stack move + owned-field drop) | Single function synchronous rollback |
| `EpochGate::observe` | 1 / core | **~2.6 ns** | Monotonic ordering & reboot re-handshake |
| `classify_stall(wall, cpu, wb, cb)` / `classify_stall_ceiling(.., ceiling)` | 1.5 / core | ~2.0 ns (const math, no clock reads) | SPIN vs SLEEP vs DEADLOCK verdict from one sample pair (+ silence ceiling) |
| `WorkerPulse::kick` + `PulseWatch::observe` | 1.5 / core | 8 B, 1 atomic store | Heartbeat: missed pulse = de-energize past thread, never kill it |
| `SystemClock` (`MonotonicMs`) | 1.5 / `std` | 1 `Instant` read | Boot-relative monotonic ms; NTP/DST edits never move deadlines |
| `node.cpu_budget_ms(n)` + `node.stall_ceiling_ms(n)` + `guard.with_cpu_sampler(f)` | 1.5 / std | 1 fn call at arming | CPU-spin budget + silence ceiling; `check_timeout` settles spin/deadlock distinctly |
| `guard.stall_kind()` | 1.5 / std | 1 clock + 1 sample | Non-destructive hang verdict (no settle) |
| `guard.check_timeout()` | 1.5 / std | 1 clock + 1 sample | Cooperative watchdog: `WithinBudget` / `TimedOut` / `SpinDetected` / `DeadlockSuspected` |
| `IsrBridge::try_push/pop` + `trip/take_trips` + `IsrSignal` | 1.5 / core | 80 B, ~2 bus cycles | ISR signal into main loop; trip latch bypasses a full ring; no mutex/alloc/`unsafe` (M0-safe byte atomics); `head`/`tail` separated by 64 B cells, no `align(64)` pads |
| `IsrSignal::new(opc, reason, value)` | 1.5 / core | 4 B, const | Pack `OP_EMERGENCY_TRIP` / `OP_PULSE` / `OP_PAUSE` |
| `IdlePark::park` (`SpinPark`/firmware `wfi`/`wfe`) | 1.5 / core | structural: 0% CPU by design | 0%-CPU park: `Condvar` on `std`, `wfi`/`wfe` impl on firmware (`wfe` needs `SEVONPEND=1` at boot) |
| `WdtSink::feed` / `NoWdt` | 1.5 / core | structural: no-op on std | Stroke HW WDT on firmware (same `kick()` feeds it) |
| `node.note_pulse_stall(lag_ms)` | 1.5 / std | structural: 1 atomic max | Record missed pulse into `SnapshotV1` (pulse fields since v3; frame is v5) + `AnomalyKind::PulseStall` |
| `node.note_memory_kb(used_kb)` + `memory_verdict()` | 3 / std | 1 lock + 1 atomic store | External RSS sample → `MemoryVerdict` (Ok/Soft/Hard); admissions refuse lock-free above soft |
| `MemoryWatch` + `MemoryVerdict` | 4 / core | 32 B, `Copy` | Verdict math for firmware; `hard < soft` clamps up |
| `MemoryPolicy::new(soft, hard)` + `note(used)` | 4 / std | pure integer compares | Supervisor verdict: soft drains, hard kills; `death_cause()` names `MemoryLimit` |
| `Supervisor::run_supervised_guard_with_memory(..)` | 4 / std | 1 sampler call / 10 ms tick | Lease + RSS watchdog; soft logs `MEMORY_DRAIN`, hard `kill()` without draining |
| `node.transition(&mut s, next, rb, work)` | 2 / std | **~90 ns** | ONE enum state machine, no flag drift |
| `node.transition_checked(s, rb, work)` + `LegalMove` | 2 / std | same + 0 ns | Closed graphs: illegal edges rejected at compile (E0277) |
| `node.enter_with_priority(prio)` | 2 / std | **~87 ns** | Critical over Background yields cooperatively |
| `Protected::new(res, guard)` + `commit()` | 1 / core | 0 ns (generics) | Resource burns with guard unless committed |
| `node.scoped_sealed(rb, work)` + `ScopeToken` | 2 / std | same as `scoped` | `!Send` seal: thread-leak fails at compile |
| `StaticNode::<I, Q>::build(name)` | 2 / std | same as builder | Capacities validated FIRST in `build()` |
| `gateway::<T>(cap)` + `host.recv()` | 2 / std | **~50.8 ns** send+receive roundtrip (the row measures BOTH halves; a bare `try_send` is cheaper and not what this number claims) | Multiple senders, one receiver channel; HWM/LWM throttle paces at 75%/50% with integer `retry_after_ms`, `send_critical` bypasses |
| `RestartTracker::note_death(code, now)` | 4 / std | pure math | Process respawn backoff tracker, caller sleeps |
| `StateDigest` + `take_state()` | 4 / core | 64 B, ~1 ns | Hot handoff: memory survives death |
| `NoOrphanGuard` / `OrphanGuard` | 4 / std | trait only | Parent death = child processes reaped (app impl) |
| `node.enter_auto` | 2 / std | **~93 ns** | Monotonic auto sequence ticket minting |
| `enter_auto_profiled` + `checkpoint_timed` + `commit_profiled` | 2 / std | 1 `Box` at arm + 1 clock read/stage | Stage wall/CPU breakdown (opt-in; default path untouched) |
| `enter_async_auto_profiled` | 3 / std | 1 `Box` + 2 clock reads | Profiled async admission; arms on the admitting poll |
| `guard.profile_snapshot()` | 2 / std | 1 wall + 1 CPU read | Mid-flight profile without settling |
| `ConflateSender::pair()` + `publish` / `take_latest` | 4 / std | **~21.7 ns** pair; ~15.7 ns replace | Latest-value channel (queue 0 or 1), coalescing counted, no `Clone` bound on payload |
| `ExecutionEnvelope::new(..)` + `execution_envelope(env)` + `commit_enveloped(bytes)` | 4 / std | 16 B `Copy` header; **~142 ns** settle with payload judge | Corridor catches too slow/too big and too fast/too small; underflow/overflow in own stats columns, breaker untouched |
| `ExecutionEnvelope::{off, is_off, is_inside, has_underflow, has_overflow, time_verdict, payload_verdict}` | 4 / std | `const fn` header math on one `Copy` value | Inspect a corridor without admitting: `off()`/`is_off()` disable it, `is_inside()` answers the four-bound question, `time_verdict(wall_us)` / `payload_verdict(bytes)` classify one measurement into `TimeVerdict` / `PayloadVerdict` |
| `ProfileHarness` + `HarnessReport` / `run_boundary_harness` | 4 / std | one `Vec` of samples + sort | Deterministic PRNG, min/median/max/jitter/burst matrix, `report.envelope(headroom)`; accessors: `HarnessReport::mean_us()`, `ProfileHarness::{samples, burst_len}`; `FastPrng` is the xorshift32 payload generator behind `run` |
| `snapshot.diff(&later)` + `write_summary` | 5 / cold | pure `Copy` math | Counter deltas + memory/queue window (`SnapshotDiff`) |
| `write_triage(&mut out, node, &snap)` | 5 / cold | `fmt::Write`, no alloc | One ASCII verdict card per node |
| `node_passthrough_enter` | 2 / std canary | **~55 ns** | Open-doors transit: lock-free count + unstamped dummy guard, no slots/breaker/`EpochGate`/clock |
| `node_passthrough_scoped` | 2 / std canary | **~51 ns** | Same transit through `scoped()` — work runs, books stay clean |
| `contended_node_enter_threads_6` (6 threads, shared `enter_auto` admit counter) | 2 / std contended | **~971 ns** | MESI cache-line bouncing on node mutex; StaleEpoch losers bounded-retry (cap 64 + yield), same contract as `concurrency_stress` |
| `contended_node_scoped_threads_6` (6 threads) | 2 / std contended | **~923 ns** | Full transactional cycle under contention (entry + work + auto-commit) |
| `contended_gateway_mpsc_threads_6` (6 producers + drain) | 2 / std contended | **~184 ns** | Bounded multi-producer write under parallel core flood |
| `cross_thread_isr_bridge_push_pop` (push core + pop core) | 1.5 / core inter-core | **~40 ns** | Lock-free SPSC ring across physical cores (honest inter-core bus latency, not L1-hot loop) |
| `node.enter_async_auto` | 3 / std | 0% CPU waker park | Async admission under bounded concurrency |
| `guard.settle_async(fut)` | 3 / std | zero lock during await | Commit across `.await` points |
| `TaskConduit` / `TaskHandle` | 2 / std | lock-free atomics | 2-way live steering, pause/resume waker, drafts |
| `PipeDrain` + `ProcessToken` | 4 / std | 44-byte stream; **~44 ns/frame** (256-frame sync drain) | Hardware rollback across process death (`kill -9`) |
| `ProcessToken::commit(task)` / `from_bytes` / `to_bytes` | 4 / core | **~7.4 ns** parse / **~7.9 ns** frame; `commit` is `const` | 44-byte wire frame, no allocation; `commit(task)` builds the settle-clean marker (`OP_COMMIT`, zero payload) without touching the wire codec |
| `node.snapshot_v1()` + `encode()` | 5 / cold | 512 B, CRC32 | Crash-safe persistence, app-owned flush thread |
| `find_anomalies()` / `assert_no_anomalies!` | 5 / cold | pure `u128` math | CI gate + diagnostics console + `[DIAG 0x…]` bug reports |
| `NodeSet` set | 5 / cold | explicit `&Node` list + saturating aggregates | Multi-node pulse, never a `static` |
| `SafeLockExt` (`Mutex::safe_lock(&node)`) | 1 / std | structural: 1 counter on recovery | Poison-tolerant locking: foreign panic recovers the lock, `poison_recoveries` counts each recovery |
| `ClosurePolicy::new(closure)` + `run()` | 2 / alloc | 1 `Box` at construction | Capturing self-healing command: `run()` executes the boxed closure outside the node lock |
| `TransactionHeader { generation, epoch, task_id }` | 1 / core | 24 B `Copy` | Stamped header for trust boundaries; `task_id` doubles as the reply-correlation check |
| `GatewayStats` | 2 / std | plain `Copy` counters | Gateway channel books: `sent` / `admitted` / `shed` / `throttled` (saturating) |
| `ResumeFuture` | 3 / std | 0% CPU waker park | Future resolved by the next directive (pause/resume/cancel); reads the directive under the wakers lock, never duplicates a payload |
| `PulseTick` (type alias) | 1.5 / core | structural: one atomic | Internal tick storage type behind `WorkerPulse` (a private detail surfaced through `WorkerPulse::observe`): `AtomicU64`, or `AtomicU32` wrapping at 2^32 on targets without 64-bit CAS. Not a public API — drive pulses through `WorkerPulse` |
| `EnvelopeVerdict`, `crc32_ieee`, `diagnostic_hash`, `scan_anomalies_for_snapshot`, `find_anomalies_for_snapshot`, `write_diagnostic_line`, `write_health` (Layer 5 helpers) | 5 / std (`find_anomalies_for_snapshot`: alloc) | `Copy` verdict / pure fns / `Write` renderers | Frame-envelope verdict types and the anomaly-scan/hash/render helpers the pool and diagnostic layers are built on; exposed for harnesses that reuse the same classification outside the crate |
| `LeaseEvent { task_id, stage, kind }` | 3 / std | `Copy` event | Reaper mailbox payload fired by `AsyncLease` drop; overflow accounting goes through `LeaseChannel::fire` |
| `run_supervised_guard_with_grace(worker_flag, lease_ms, eof_grace_ms, on_dead_token)` | 4 / std | one reaped child process | Supervised guard with a configurable terminal EOF grace: a slow-flushing child (big buffered stdout) gets more than the default grace before death classification, so an in-flight rescue token is never misreported CLEAN |
| `ProcessToken` / `SnubScanner` (core `drain` module) | 4 / core | 44-byte wire frame; zero allocation | Token parser reachable without `std`: `ProcessToken::commit` / `to_bytes` / `from_bytes` / `from_body_bytes` and the streaming `SnubScanner::push` work on bare metal |
| `PipeDrain::feed` / `finish` / `write_to` (transport) | 4 / std | `feed`: caller-pushed chunks, no reader thread; `write_to` needs `std::io::Write` | Chunk transport layered ON the core scanner: `feed` takes raw bytes (UART/DMA adapters call it directly), `write_to` requires a `std::io::Write` sink — on bare metal implement the adapter yourself |
| `lane.attach(..)` / `lane.note_death(..)` → `PoolReap` | 4 / std | **~225 ns** ambush / **~187 ns** escape (one lane lock each) | One reap: `class` (`Ambush`/`Escape`/`Stillborn`/`Deadlocked`), rescued `token`, next `generation`, last `StateDigest`, per-lane `RestartDecision` (`respawn_delay_ms()`); the app owns sleep + spawn |
| `PoolConfig::{with_memory, with_eof_grace}` + `WorkerPool::new` | 4 / std | `const` builder; one `Vec<Arc<PoolLane>>` | Pool configuration: per-lane soft/hard RSS limits and the terminal EOF grace (default `DEFAULT_EOF_GRACE_MS`); `new` validates capacity and returns `PoolError` |
| `PoolLane::{next_epoch, pid, counters, idle_ms, expired, memory_verdict, snapshot}` | 4 / std | one short lane lock per call | Lane introspection: next generation ticket (handshake value), current pid, `DrainCounters`, idle time, lease expiry, last `MemoryVerdict`, and a `LaneSnapshot` row |
| `WorkerPool::total_rescued` / `PoolReport::{deadlocked, conserved, healthy, final_generations}` | 4 / std | saturating fold over lane snapshots; one `Vec<u64>` | Pool-wide rescue count; `deadlocked()` = any lane had an unverifiable pipe; `conserved()` = every lane books its reaps; `healthy()` = both; `final_generations()` = the generation each lane stopped at (`kills + 1` on a clean storm) |
| `pool.report()` / `PoolTotals::of(&lanes)` | 4 / std | **~107 ns** four lanes | Aggregated row per lineage (`LaneSnapshot`) plus pool-wide sums; `report()` performs one `Vec<LaneSnapshot>` allocation (`lanes.len()` rows), included in the measurement |
| `pool.expired_lanes(lease_ms)` | 4 / std | **~5.4 ns** four lanes (~130 ns per lane) | Lease sweep: names only lanes whose parsed heartbeat froze past the lease; the app kills them itself |
| `LaneSnapshot` / `PoolTotals` | 4 / std | `Clone` row / `Copy` totals | Rows carry `conserved()` (`ambush+escape+stillborn+deadlocks == reaps`); totals fold the same four honest buckets, never a guessed fifth |
| `DEFAULT_EOF_GRACE_MS` (250 ms) | 4 / std | published const, compile-asserted | Default terminal EOF grace; `run_supervised_guard_with_grace` and the pool builder override it per call/site |
| `Supervisor::run_supervised_guard_with_lease_argv(..)` / `..._with_memory_argv(..)` | 4 / std | explicit `&[String]` argv | Argv twins of the lease/RSS watchdogs for tests, embedded launchers and shells that parsed their own command line (index 0 is the program name) |
| `DrainCounters { frames, arms, commits }` | 4 / std | `Copy`, updated per parsed frame | Drain-side frame census: `commits > 0` means the worker settled at least once; `arms > commits` means it was armed more often than it settled (`OP_SNAPSHOT` frames are diagnostics, never counted as arms) |
| `TokenRollback` (`fn(u32, [u8; TOKEN_PAYLOAD_LEN])`) | 4 / std | bare fn pointer | Emergency handler invoked in the supervisor when a dead child leaves an armed token; fast, idempotent, lock-free |
| `ProfiledEnterFuture` | 3 / std (opt-in) | 1 `Box` + clock reads | Future returned by `enter_async_auto_profiled`: same waker parking; profile arms on the admitting poll |
| `backpressure_of(&EnterError)` | 2 / std | pure | `Some(Backpressure)` (retry-after + queue depth) for capacity refusals, `None` when retrying cannot help |
| `GatewaySender<T>` / `GatewayHost<T>` | 2 / std | sender `Clone`; host unique | Channel gateway endpoints from `gateway(capacity)`: signed depth books, HWM/LWM throttle; the host runs the consumer loop |
| `TOKEN_MAGIC` / `SNAPSHOT_MAGIC` | 4 + 5 / core | `[u8; 4]` (`SNUB`) | Wire and snapshot magic constants. Same literal, separate contracts: token framing vs the 512-byte snapshot header |
| `LANE_SHIFT` (48) / `TASK_LOCAL_MASK` | 4 / core | published consts | Task-id packing: `lane << LANE_SHIFT \| (local & TASK_LOCAL_MASK)`; use `namespace_task` / `task_lane` / `task_local` |
| `STATE_LEN` (64) | 4 / core | const, compile-asserted | Size of `StateDigest`; streamed as three `OP_STATE` frames (24 + 24 + 16) |

