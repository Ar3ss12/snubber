# Benchmarks

The single home for measured latency figures in the public docs. This file
carries the full operation matrix, the contended group and the
termination-storm tables; README.md and docs/USER_GUIDE.md link here and
quote no numbers. The API & cost
quick reference — signatures, layers and footprints, no measurements —
lives at the end of docs/USER_GUIDE.md. `tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact`
checks every `~<number>` citation below against the committed snapshot
[`docs/evidence/2026-10-30_bench_snapshot.md`](docs/evidence/2026-10-30_bench_snapshot.md)
— the docs never carry a second copy of a measurement.
`tests/doc_sync.rs::test_bench_rows_quote_their_own_measurement` is the
second half: it pairs each row with the snapshot row it names and requires
every citation there to round or truncate from THAT measurement. An
existence check alone is not enough — it let
`pool.expired_lanes(lease) (4 lanes)` publish 5.4 ns (another row's
number) for four days while its own sweep measured 516 ns.

Measured on AMD Ryzen 5 4600H release build
(`cargo bench --bench admission_bench`, Criterion: 800 ms warmup,
2 s measurement, 50 samples, isolated node per bench). Values are the
second of two full 64-row runs on 2026-10-30. The runs agreed row-for-row
outside the contended group — median absolute delta 0.8 % — and the
contended group moves by design.
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
124 → 91 ns, `node_passthrough.enter + commit` 96.5 → 55 ns. The
2026-10-30 pass measured what had been published as structure only: two
new bench groups, `layer2_watchdogs` (`MemoryTrend::sample`,
`MemoryTrend::verdict`, `HandleWatch::sample`, `jitter_ms`, `AimdLimit`)
and `layer5_cold_evidence` (`FlightTape`, `snapshot_v1.decode`,
`snapshot_v1.diff`, `write_triage`, `RankedSet::insert`), which took the
exporter label map from 52 keys to 64 and the table from 53 rows to 64.
Five rows also moved enough to be more than noise against the 2026-10-26
artifact: `probe_erased_auto + commit` 205.4 → 168.2 ns (the admission
kind moved into the guard's type in `79c396e`, so the probe path no
longer branches on it), `static_node.enter + commit` 125.1 → 89.8 ns,
`guard.commit_enveloped(64 B)` 142.5 → 116.6 ns, `enter_async_auto`
park/wake 542.2 → 474.0 ns and the ready poll 156.0 → 151.4 ns. Only the
erased row has an attributed cause; the other four are re-measurements
across the 0.7.1 → 0.7.6 span and no single commit is claimed for them.
`cargo bench` regenerates the whole table into
`target/criterion/*/new/estimates.json`. The scheduled `bench-full` CI
lane runs `cargo bench --locked --bench admission_bench`, then
`bash scripts/bench.sh --skip-run --github` to render
`target/bench/snapshot.md` — generated output inside the gitignored
`target/` tree, not a file in this repository — and archives that file
plus the raw Criterion trees as artifact `bench-criterion-<run_id>`. To re-print the table from an
existing `target/criterion` without running benches again, use
`bash scripts/bench.sh --skip-run`. Both exporters (`bench.sh`,
`bench.ps1`) carry the same 64-entry label map, so every published row
reads as an operation rather than a Criterion key. Differences below
2 ns are inside run-to-run noise and are not claimed as wins.

## Full operation matrix

| Operation | Architecture Layer | Average Latency | Notes |
|---|---|---|---|
| `SnubberGuard::new` + `commit` | Layer 1 (`core`) | **~5.3 ns** | Stack move + owned-field drop, zero heap allocation |
| `EpochGate::observe` | Layer 1 (`core`) | **~2.6 ns** | 2-D generation + monotonic epoch filter |
| `classify_stall_ceiling` (bench; `classify_stall` is the 4-arg wrapper that delegates here) | Layer 1 (`core`) | **~1.9 ns** (const math) | `CpuSpin` / `SleepBlocked` / `DeadlockStall` from one sample pair (the settle verdicts `SpinDetected` / `DeadlockSuspected` come from `guard.check_timeout`) |
| `WorkerPulse::kick` + `PulseWatch::observe` | Layer 1.5 (`core`) | **~5.8 ns** | One-word heartbeat store + observe fold; no lock, no allocation |
| `IsrBridge::try_push` + `try_pop` | Layer 1.5 (`core`) | **~7.1 ns** | 80 B SPSC ring, lock-free; malformed cells drained on pop |
| `IsrBridge::trip` + `take_trips` | Layer 1.5 (`core`) | **~9.4 ns** | 32-bit latch swap; trip bypasses a full ring |
| `node.enter + commit` | Layer 2 (`std`) | **~91 ns** | Full bulkhead, circuit breaker, auto-commit; no clock read when neither the token bucket nor a deadline is armed (unstamped guard) |
| `node.enter_auto + commit` | Layer 2 (`std`) | **~96 ns** | + atomic sequence minting |
| `node.scoped (work + auto-commit)` | Layer 2 (`std`) | **~90 ns** | Canonical closure entry: admission + work + auto-commit in one call |
| `node.enter_with_priority(Critical)` | Layer 2 (`std`) | **~93 ns** | Priority lane; same band as plain enter (1.9 ns apart, inside noise) |
| `node.transition` | Layer 2 (`std`) | **~93 ns** | ONE-enum state machine, zero flag drift |
| `node.transition_checked` + `LegalMove` | Layer 2 (`std`) | same path, no extra work | Compile-time only: the `LegalMove` proof erases to the same `transition` call; illegal moves fail at compile (E0277) |
| `probe_erased_auto` + `commit` | Layer 2 (`alloc`) | **~168 ns** | ONE boxed transaction (rollback + commit hook + node clone in a single allocation; 0.6.8 moved the breaker outcome onto the guard's real `commit`). The admission kind is in the guard's type since `79c396e`, so the probe path no longer branches on it: 301.6 ns (0.6.8) → 205.4 ns (2026-10-26 artifact) → 168.2 ns now |
| `node_passthrough.enter + commit` | Layer 2 (`std` canary) | **~56 ns** | Open-doors transit: lock-free count + unstamped dummy guard, no slots/breaker/`EpochGate`/clock |
| `node_passthrough.scoped + commit` | Layer 2 (`std` canary) | **~51 ns** | Same transit through `scoped()` — work runs, books stay clean |
| `ConflateSender::publish + take_latest` | Layer 2 (`std`) | **~21.4 ns** | One-slot latest wins, no allocation, no `Clone` bound |
| `ConflateSender::publish` (occupied slot) | Layer 2 (`std`) | **~15.6 ns** | Replaced value counted, never queued |
| `guard.commit_enveloped(64 B)` | Layer 2 (`std`) | **~117 ns** | Payload corridor exact; wall corridor only for profiled guards |
| `TaskConduit::check_directive` | Layer 2 (`std`) | **~1.0 ns** | One atomic load per poll, no lock |
| `gateway send -> recv roundtrip` | Layer 2 (`std`) | **~50.1 ns** | Bounded MPSC send plus receiver wake |
| `gateway send` (HWM/LWM throttled) | Layer 2 (`std`) | **~16.6 ns** | Latch fast path while inside the high/low horizon |
| `node.next_sequence (auto ticket)` | Layer 2 (`std`) | **~4.8 ns** | One atomic `fetch_add` ticket, shared across clones |
| `node.stats()` snapshot | Layer 2 (`std`) | **~58 ns** | One short lock, fold into `SnubberStats` |
| `SnubberStats::shed_total + success_rate_per_mille` | Layer 2 (`std`, cold) | **~29 ns** | One fold over the stats card: shed totals and the success permille |
| `MemoryTrend<10>::sample` | Layer 2 (`std`, cold) | **~9.6 ns** | One push into the RSS ring per 30 s sample: shift, subtract, overwrite; no lock, no allocation |
| `MemoryTrend<10>::verdict` | Layer 2 (`std`, cold) | **~13.9 ns** | The full fold: level, gradient over 10 slots, rate — `MemoryReport` answers both halves in one call |
| `HandleWatch<10>::sample (probe)` | Layer 2 (`std`, cold) | **~17.8 ns** | Caller's probe runs inside: one `FnMut() -> Option<u64>` call plus the ring push; a `None` probe records nothing and leaves the verdict alone |
| `jitter_ms (20 % spread)` | Layer 2 (`std`) | **~10.5 ns** | Deterministic ±20 % spread from a caller salt: two multiplies, no RNG, no clock |
| `AimdLimit::on_success + on_pressure` | Layer 2 (`std`) | **~3.7 ns** | One ceiling pair: additive increase, multiplicative decrease, both clamped; a cycle, not a single call |
| `enter_async_auto` | Layer 3 (`std`) | **0% CPU** | Pure `core::task::Waker` parking, no runtime deps |
| `enter_async_auto_profiled` | Layer 3 (`std`, opt-in) | structural: 1 `Box` + 2 clock reads | Same waker parking, profile arms on the admitting poll |
| `enter_async_auto` (ready poll) | Layer 3 (`std`) | **~151 ns** | Full admission through the future; the ready path allocates nothing |
| `enter_async_auto` park/wake/admit roundtrip | Layer 3 (`std`) | **~474 ns** | Same-thread park (queue push + identity flag) -> slot release wake -> admitting re-poll; no executor, not a cross-core roundtrip |
| `static_node.enter + commit` | Layer 2 (`std`) | **~90 ns** | Compile-time capacity assert + bulkhead + auto-commit; the same band as `node.enter`, so the const check costs nothing at run time |
| `node.note_pulse_stall(lag_ms)` | Layer 2 (`std`) | structural: 1 atomic max | Missed heartbeat lands in `SnapshotV1` (pulse fields since v3; frame is v5) + `AnomalyKind::PulseStall` |
| `node.note_memory_kb(used)` | Layer 2 (`std`, cold) | structural: 1 lock + 1 atomic store | External memory sample → soft/hard verdict; admissions refuse lock-free above soft |
| `process_token.to_bytes()` | Layer 4 (`core`) | **~7.6 ns** | 44-byte framed wire serialization (`to_le_bytes`) |
| `ProcessToken::from_bytes()` | Layer 4 (`core`) | **~7.2 ns** | 40-byte body parse, no allocation |
| `state_digest.crc()` | Layer 4 (`core`) | **~262 ns** | 64-byte hardware cache line digest verification |
| `PipeDrain::drain_sync (256 frames)` | Layer 4 (`std`) | **~43.1 ns/frame** | 256 frames: 11.0 µs, 23.2M frames/s; the per-sample `PipeDrain` construction is included, so this is a pipeline cost, not a steady-state frame cost (see `feed` below) |
| `PipeDrain::feed (one frame)` | Layer 4 (`std`) | **~97 ns** | Caller-pushed chunk (one 44 B frame): scanner + lock + dispatch, no reader thread |
| `PipeDrain::feed (256 frames)` | Layer 4 (`std`) | **~10.8 µs** (42.3 ns/frame) | Same scanner: one ingest lock per chunk + one short state lock per token (0.6.8 took the clock read outside the lock) |
| `ProcessToken::write_to(&mut sink)` | Layer 4 (`std`) | **~2.0 ns** | 44 B `write_all` into any real `Write` (copy included): no env, no stdout lock |
| `StateDigest::write_to(&mut sink)` | Layer 4 (`std`) | **~611 ns** | Three `OP_STATE` frames + CRC32 seals |
| `namespace_task::pack` | Layer 4 (`std`) | **~1.0 ns** | `task = lane << 48 \| local`; two shifts + mask, no branch |
| `namespace_task::unpack (task_lane + task_local)` | Layer 4 (`std`) | **~1.0 ns** | The reverse: two masks, no branch |
| `lane.attach` + `note_death` (ambush) → `PoolReap` | Layer 4 (`std`) | **~198 ns** | Full rescue path: drain EOF, token take, generation bump, restart math, per-class counters (drain built in setup; the shared idle lane is re-attached each sample) |
| `lane.attach` + `note_death` (escape) → `PoolReap` | Layer 4 (`std`) | **~186 ns** | Same path when the child settled (commit parsed, no token) |
| `pool.report()` (4 lanes) | Layer 4 (`std`) | **~107 ns** | Rows (`LaneSnapshot`) + `PoolTotals::of`: four short lane locks, totals fold, one `Vec<LaneSnapshot>` allocation (`lanes.len()` rows) included in the number |
| `LaneSnapshot` / `PoolTotals` | Layer 4 (`std`) | `Clone` row / `Copy` totals | `conserved()` = `ambush+escape+stillborn+deadlocks == reaps`; no guessed fifth bucket |
| `run_supervised_guard_with_grace(..)` | Layer 4 (`std`) | one reaped child process | Supervised guard with a configurable terminal EOF grace for slow-flushing children |
| `DEFAULT_EOF_GRACE_MS` (250 ms) | Layer 4 (`std`) | published const | Default terminal EOF grace; `run_supervised_guard_with_grace` and the pool builder override it |
| `lane.note_memory_kb` | Layer 4 (`std`, cold) | **~12 ns** | Per-lane watch: one lock + integer compares |
| `pool.expired_lanes(lease)` (4 lanes) | Layer 4 (`std`, cold) | **~516 ns** | One sweep over four silent pipes: `expired()` and `idle_ms()` each take the lane state lock, so an expired lane costs two locks and two clock reads, and four pushes grow the result `Vec` through three allocations — 129 ns per lane. No child process is touched. This row read 5.4 ns until 2026-10-30: that was `SnubberGuard::new` + `commit`'s number, borrowed by a doc that only checked the value existed somewhere |
| `run_supervised_guard_with_memory` | Layer 4 (`std`) | structural: 1 sampler call / 10 ms tick | Soft logs `MEMORY_DRAIN`; hard `kill()` without draining (`DeathCause::MemoryLimit`) |
| `enter_auto_profiled` + `checkpoint_timed` + `commit_profiled` | Layer 2 (`std`, opt-in) | structural: 1 `Box` + 1 clock/stage | Stage wall/CPU breakdown; default `enter*` reads no profile clock |
| `snapshot_v1.encode()` | Layer 5 (`core`) | **~1.12 µs** | 512-byte zero-allocation snapshot CRC32 codec |
| `snapshot_v1.decode()` | Layer 5 (`core`) | **~1.12 µs** | The cold read back: frame parse plus the CRC32 re-check over the same 512 B; encode and decode cost the same because one CRC pass dominates both |
| `snapshot_v1.diff (counter deltas)` | Layer 5 (`core`) | **~22.9 ns** | `before.diff(&after)` over a full snapshot pair: pure `Copy` integer subtract-and-saturate, no allocation |
| `write_triage(card)` | Layer 5 (`std`, cold) | **~630 ns** | One ASCII verdict card into a `String` (`fmt::Write`), the same call the exit triage file makes |
| `FlightTape<6>::record_code + newest` | Layer 5 (`core`) | **~5.1 ns** | Cold-edge event: one slot write plus the generation stamp, read back with `newest()`; no allocation, no lock |
| `FlightTape<6>::last_with_code` | Layer 5 (`core`) | **~3.3 ns** | The scan a triage card does per event class: walk the ring backwards looking for one code |
| `RankedSet::insert (teardown order)` | Layer 5 (`std`) | **~24.3 ns** | `RankedNode<3>` into an 8-slot set: rank compare + shift into place. Criterion calls the batched setup outside the timed window, so the fresh set's construction is not in the number — the insert and the set's drop are |
| `friction_fn` handler call (`Arc<dyn Fn>`) | Layer 2 (`std`) | **~1.8 ns** | Capture-capable friction: one dynamic dispatch outside the node lock |
| `friction_wait_ms` (adaptive, Arc handler) | Layer 2 (`std`) | **~21.5 ns** | Lock + `Arc` clone + handler; the same band as the legacy fn-pointer path below |
| `friction_wait_ms (adaptive, fn)` | Layer 2 (`std`) | **~21.7 ns** | Same wait through a plain `fn` pointer: no clone, same band |
| `NodeName::Static` clone | Layer 2 (`alloc`) | **~1.5 ns** | Fat-pointer copy, no allocation |
| `NodeName::Owned` clone | Layer 2 (`alloc`) | **~9.9 ns** | One `Arc<str>` increment |
| `NodeName::as_str` | Layer 2 (`alloc`) | **~1.0 ns** | Deref to the borrowed str; no copy, no allocation |
| `snapshot_v1()` + `find_anomalies()` | Layer 5 (cold) | structural: 512 B, CRC32 | Zero-allocation telemetry; `assert_no_anomalies!()` CI gate. The frame codec itself is the `encode`/`decode` rows above |
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
[`max + 1`](docs/USER_GUIDE.md#layer-2-concurrency-bulkhead-bounded-queues--backpressure)
slot — beyond that it sheds like every other caller.

## Contended Multi-Threaded Latencies (Real Thread Saturation)
Measured with 6 concurrent OS threads under contention (no CPU affinity pinning), AMD Ryzen 5 4600H, full 50-sample runs of 2026-10-30 (`cargo bench --locked --bench admission_bench contended_multithreaded`; archived as `target/bench/snapshot.md` — generated by `scripts/bench.sh`, not a repository file — together with the raw Criterion trees in the CI `bench-criterion` artifact, and frozen in the artifact linked at the head of this file):

| Contended Operation | Concurrency | Measured Latency | Physical Phenomenon |
|---|---|---|---|
| `contended_node_enter_threads_6` | 6 threads | **~1.03 µs** | Mutex contention: cache-line transfer between threads |
| `contended_node_scoped_threads_6` | 6 threads | **~1.04 µs** | Full transactional cycle under contention (entry + work + auto-commit) |
| `enter_erased_holding_auto + commit (6 threads)` | 6 threads | **~1.25 µs** | Holding erased admission: slot booked until `commit`, so each iteration pays admission + boxed transaction + release; the probe path beside it frees at entry and never shows this cost |
| `contended_gateway_mpsc_threads_6` | 6 threads | **~217 ns** | Bounded multi-producer channel write under parallel thread load |
| `cross_thread_isr_bridge_push_pop` | 2 threads | **~31 ns** | Lock-free SPSC ring: cache-line handoff between threads |

The two full 50-sample runs of 2026-10-30 read 1.03 / 1.03 µs, 1.04 / 1.04 µs, 1.25 / 1.25 µs, 192.9 / 217.2 ns and 34.0 / 31.2 ns for the same five rows — the contended group moves between runs by design; quote full-run artifacts, not smoke checks. A saturated node admits 0.97M admissions/sec under 6-thread contention (one 6-thread cycle per 1.03 µs); the uncontended admission row above is 11 times cheaper, and that ratio is the whole argument for the lock-free Layer 1.5 paths.

## Multi-Lineage Termination Storm (out-of-process rescue)

`scripts/stress_multi_lineage.ps1` runs N supervisors in parallel, each with its own worker, pid file, pipe and raw Win32 killer. One supervisor = one lineage: isolation needs no library changes, because each lane runs in its own working directory (`-LaneRoot`) and the worker publishes the RELATIVE path `target/supervised_worker.pid`, so the pid file is private to the lane. `-ArmedHold` parks the worker armed and publishes its pid only after the arm frame, so an ambush-rate gate cannot be satisfied by shooting processes that never armed. `scripts/stress_multi_lineage.py` is the same harness for Linux and macOS: `SIGKILL` instead of `TerminateProcess`, process identity from `/proc/<pid>/stat` starttime (Linux) or `ps -o lstart` (macOS), so a recycled pid is never mistaken for the live worker. Python 3 standard library only, no dependencies. Both harnesses emit one report schema — `report_schema: 2`, same keys in the same order — and `test_storm_harnesses_publish_one_report_schema` pins that parity, because one oracle has to read a Windows artifact and a POSIX artifact without branching on which harness wrote it.

Each lane samples its supervisor's working set, private bytes, handle count and thread count once per poll, and a poll that returns an incoherent tuple is dropped and counted (`sample_drops`) instead of recorded. The reason is a concrete failure: .NET answers `HandleCount` with 0 for a process that exited between the refresh and the read, while the cached memory fields still report the last live values, and that fabricated 0 lands in the growth arithmetic as a −68-handle drop. A lane therefore fails the `resource window readable` gate unless it can show a first and last reading on both counters. Histogram keys are disjoint band counts (`band_200` = kills whose dispatch landed in `(100, 200]` µs), so the nine bands sum to `kills_completed`; seven percentiles cannot show whether a 14 ms max was one kill or a thousand, and this is the field that tells.

The three runs below are the 2026-10-31 Win32 storms, release profile, 10,000 `TerminateProcess` dispatches each, quoted from the committed reports — verdict `PASS` on all 48 / 43 / 92 gates (11 per lane under `-ArmedHold`, 10 without it, plus 4 or 3 aggregates), with the console transcript of each run committed beside it:

| Mode | Shape | Wall | Kills/s (avg / peak) | Dispatch p50 / p99 / max | Ambush | Escape | Stillborn |
|---|---|---|---|---|---|---|---|
| `-ArmedHold` | 4 × 2,500 | 25.82 s | 387.2 / 453.3 | 374.7 / 1,360.6 / 16,214 µs | 10,000 (100.0%) | 0 | 0 |
| `-Aggressive` (free-running) | 4 × 2,500 | 14.13 s | 707.6 / 826.8 | 413.6 / 1,839.6 / 30,937 µs | 1,999 (20.0%) | 77 (0.8%) | 7,924 (79.2%) |
| `-ArmedHold` at 8 lanes | 8 × 1,250 | 17.43 s | 573.6 / 726.5 | 318.4 / 5,024.2 / 44,112 µs | 10,000 (100.0%) | 0 | 0 |

- **armed-hold** (4 lineages × 2,500 kills, wall 25.82 s) — per-lane dispatch p50 365.2…382.6 µs, p99 1,209.7…1,537.5 µs; private-memory growth +0.72…+0.97 MB; handle growth -1…+2; samples 200…202 per lane with 0 dropped reads; `final_generation` 2,501 = kills + 1; `supervisor_exit` 0.
  Bands (µs, disjoint, sum 10,000): 0-50: 0 | 50-100: 0 | 100-200: 988 | 200-500: 7,370 | 500-1000: 1,489 | 1000-2000: 107 | 2000-5000: 41 | 5000-10000: 0 | >10000: 5.
  Killer counters: 0 stale pids, 4,043 duplicate pid-file reads, 20,600 retries, 531 pid-miss polls across 4 lineages.

- **free-running** (4 lineages × 2,500 kills, wall 14.13 s) — per-lane dispatch p50 404.1…417.8 µs, p99 1,648.3…1,917.1 µs; private-memory growth +0.76…+1.03 MB; handle growth -1…+5; samples 107…108 per lane with 0 dropped reads; `final_generation` 2,501 = kills + 1; `supervisor_exit` 0.
  Bands (µs, disjoint, sum 10,000): 0-50: 0 | 50-100: 0 | 100-200: 664 | 200-500: 6,500 | 500-1000: 2,637 | 1000-2000: 127 | 2000-5000: 61 | 5000-10000: 2 | >10000: 9.
  Killer counters: 0 stale pids, 1,506 duplicate pid-file reads, 7,560 retries, 596 pid-miss polls across 4 lineages.

- **saturated** (8 lineages × 1,250 kills, wall 17.43 s) — per-lane dispatch p50 299.7…334.0 µs, p99 4,018.0…5,894.0 µs; private-memory growth +0.23…+0.47 MB; handle growth -1…+4; samples 96…100 per lane with 0…1 dropped reads; `final_generation` 1,251 = kills + 1; `supervisor_exit` 0.
  Bands (µs, disjoint, sum 10,000): 0-50: 0 | 50-100: 0 | 100-200: 498 | 200-500: 7,016 | 500-1000: 1,727 | 1000-2000: 227 | 2000-5000: 431 | 5000-10000: 89 | >10000: 12.
  Killer counters: 0 stale pids, 6,637 duplicate pid-file reads, 12,831 retries, 416 pid-miss polls across 8 lineages.


The 8-lane run is what the extra lineages cost a 6-core host: the same 10,000 kills, twice as many supervisors, pipes and killers, and the dispatch p99 goes from 1,360.6 µs to 5,024.2 µs while the average rate goes up (387.2 to 573.6 kills/s) — contention shows in the tail, not in the mean. The free-running run shows what the rate buys back: at 707.6 kills/s most shots land while the process is still starting, so 7,924 kills never reached an armed window and `stillborn` is the honest bucket rather than a lost token. `wall`, `kills/s`, dispatch percentiles and handle growth are wall-clock observations that move between runs; the band counts, generations, stale-pid totals and the conservation gates are deterministic. Machine, taken from the `system` block every report carries: AMD Ryzen 5 4600H with Radeon Graphics, 6 physical / 12 logical cores, Microsoft Windows 10 Pro Education build 10.0.19043.0, rustc 1.99.1 (58cf8f9c3 2026-10-13), PowerShell 5.1.19041.906, release profile.

Rescue accounting, per report: armed-hold reads `tokens_rescued` 10,000 of 10,000 kills, `clean_settles` 0; the saturated run reads `tokens_rescued` 10,000 of 10,000 kills, `clean_settles` 0; the free-running run reads `tokens_rescued` 1,999 of 10,000 kills, `clean_settles` 8,001. `tokens_rescued` equals `true_ambush` in every report and `clean_settles` equals `clean_escape` + `stillborn`, so the two counters partition `kills_completed` — that is what the harness gate `aggregate rescue + clean == reaped` checks, and it is the reason a high ambush rate cannot be manufactured by dropping tokens: an unsettled kill shows up as a missing rescue, not as silence.

Artifacts: [`docs/evidence/2026-10-31_multi_lineage_armedhold_report.json`](docs/evidence/2026-10-31_multi_lineage_armedhold_report.json) with [`_run.log`](docs/evidence/2026-10-31_multi_lineage_armedhold_run.log), [`docs/evidence/2026-10-31_multi_lineage_freerun_report.json`](docs/evidence/2026-10-31_multi_lineage_freerun_report.json) with [`_run.log`](docs/evidence/2026-10-31_multi_lineage_freerun_run.log), [`docs/evidence/2026-10-31_multi_lineage_saturation_report.json`](docs/evidence/2026-10-31_multi_lineage_saturation_report.json) with [`_run.log`](docs/evidence/2026-10-31_multi_lineage_saturation_run.log). They supersede the 2026-10-30 storm set, which ran the battery before the `armed_hold matches flag` shape gate existed and stays committed as the record of that 44 / 39 / 84-gate tree; `test_storm_tables_match_committed_reports` refuses a doc that cites it as current evidence. The 2026-10-27 pair stays committed as the record of that tree. The per-lane raw dispatch series of these runs (`_stats.json`) is not committed: 10,000 wall-clock samples per report, and this file quotes no number the report does not already carry — the 2026-10-27 `_stats.json` pair, by contrast, is a byte-identical duplicate of its report, because that harness wrote one payload to two paths. [`docs/evidence/README.md`](docs/evidence/README.md) records the exact command behind every committed artifact.

The same gates run in CI (`.github/workflows/stress.yml`, `--aggressive`, dispatch latency = `SIGKILL` call only; runner-local figures read from the workflow annotations `scripts/ci_storm_summary.py` publishes, which is the only channel an anonymous request can reach — the step log and the artifact zip of a public repository are not. Every cell below is from Stress run #149, commit `7fc5aeb`, the first run whose annotations carry the dispatch percentiles):

| Runner | Mode | Lanes × kills | Kills | Wall | Kills/s (avg / peak) | Ambush | Escape | Stillborn | p50 / p99 / max |
|---|---|---|---|---|---|---|---|---|---|
| ubuntu-latest | armed-hold | 4 × 250 | 1,000 | 0.481 s | 2,078.4 / 2,075.7 | 1,000 (100.0%) | 0 | 0 | 5.3 / 21.6 / 95.2 µs |
| ubuntu-latest | saturation | 8 × 125 | 1,000 | 0.465 s | 2,152.0 / 2,191.2 | 1,000 (100.0%) | 0 | 0 | 4.8 / 86.1 / 240.5 µs |
| ubuntu-latest | free-running | 4 × 500 | 2,000 | 1.147 s | 1,744.0 / 1,803.7 | 1,895 (94.8%) | 45 | 60 | 4.6 / 13.7 / 1,013.5 µs |
| macos-latest (arm64) | armed-hold | 4 × 250 | 1,000 | 5.07 s | 197.1 / 224.7 | 1,000 (100.0%) | 0 | 0 | 20.1 / 388.5 / 517.5 µs |
| macos-latest (arm64) | saturation | 8 × 125 | 1,000 | 4.65 s | 214.8 / 226.7 | 1,000 (100.0%) | 0 | 0 | 54.2 / 553.2 / 1,147.8 µs |
| macos-latest (arm64) | free-running | 4 × 500 | 2,000 | 29.93 s | 66.8 / 76.8 | 1,275 (63.8%) | 725 | 0 | 11.9 / 64.3 / 842.7 µs |

All six verdicts are `PASS` on `report_schema: 2`. #148 (`8bcb0d6`) is the first POSIX completion of that schema — the two runs before it (`#146`, `#147`) died in the harness, before any report was written — and the two runs disagree on the clock, which is the point of naming the run: macOS armed-hold read 84.9 kills/s in #148 and 197.1 in #149, and the Linux column moved from 2,232.8 to 2,078.4 on the same shape. In #149 the macOS rows are 10–26 times slower than the Linux ones (armed-hold 197.1 against 2,078.4, free-running 66.8 against 1,744.0); nothing in the annotation says why, and these are runner observations on a shared hosted image, not library measurements. The bucket counts and the conservation identities hold in every run.

On POSIX every push runs all three shapes; the Windows multi-lineage job (armed-hold and saturation) and the single-process PS harness are scheduled and manual-dispatch only, which is why the Windows figures above come from the runs committed in `docs/evidence/` rather than from a runner. The percentiles are the `SIGKILL` call only, so the macOS `max` of 1,147.8 µs is scheduler noise on a shared host, not a snubber path; the Win32 storms in the table above use the same column for `OpenProcess` + `TerminateProcess` and are not comparable to this one. The average is `kills_completed` over wall, the peak is the largest 250 ms window of `rate_window_per_sec` — two different measurements, not one number printed twice: the ubuntu armed-hold run lasts 0.48 s, about two windows, and its 2,075.7 peak sits just below the 2,078.4 mean. No cell was rounded toward the other, and a peak above the mean on a longer run (free-running, 1.15 s) is the same two quantities diverging the other way.

```powershell
# Single-lineage release gate (10,000 kills)
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 10000 -Launch -RawKill -Aggressive
# Armed-hold rescue proof (true ambush >= 70%)
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 1000 -Launch -RawKill -Aggressive -ArmedHold
# 4 independent lineages, 2,500 kills each
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold
# 8 lineages x 1,250 kills on the same host — the saturation run
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 8 -KillsPerLine 1250 -Aggressive -ArmedHold
```

```bash
# Linux / macOS: same storm, SIGKILL path, JSON gates
python3 scripts/stress_multi_lineage.py --lines 4 --kills-per-line 2500 --aggressive --armed-hold \
    --report target/multi_lineage_report.json --stats target/multi_lineage_stats.json \
    --log target/multi_lineage_run.log
```

## Worker-Pool Storm (one pool process, four lanes)

`examples/19_worker_pool` runs the same four-bucket storm with the pool process itself owning `spawn` / `kill` / respawn for every lane: one process, N lanes, N independent pipes, and a monitor thread calling `pool.report()` + `WorkerPool::expired_lanes()` throughout, so a torn accounting window is observed during the storm rather than inferred from the final card. Three `#[ignore]`d gates drive it — `tests/supervisor_pool_stress.rs`, `_lease.rs`, `_mixed.rs` — and each writes the one-line JSON committed here. Values are counts, not wall-clock observations, so `wall_ms` is the only column that moves between runs:

| Harness | Lanes | Reaped | Rescued | Ambush / escape / stillborn | Deadlocks | Conservation | Wall |
|---|---|---|---|---|---|---|---|
| `supervisor_pool_stress` | 4 | 32 | 32 | 32 / 0 / 0 | 0 | `conserved: true`, 0 breaks | 52 ms |
| `supervisor_pool_stress_lease` | 4 | 24 | 0 | 0 / 0 / 24 | 0 | `conserved: true`, 0 breaks | 195 ms |
| `supervisor_pool_stress_mixed` | 4 | 32 | 8 | 8 / 8 / 8 | 8 | `conserved: true`, 0 breaks | 471 ms |

The lease run is the one that measures the observer: 24 children send exactly one heartbeat and go silent, so every reap is `Stillborn` (a heartbeat proves the pipe, not a settlement), `lease_kills` = 24 and the monitor completed 102,471 `report()` + `expired_lanes()` checks with 0 conservation breaks and 0 `token_lane_mismatch`. The mixed run cycles each lane through `armed → clean → silent → orphan`, which is the only shape that produces all four buckets in one process; the `orphan` leg leaves a grandchild holding the pipe open, and the run reporting `deadlock: true` with 8 deadlocks is the honest bucket — a lane that never settles is named, not counted as a rescue. Tokens: `tokens_seen` 32 / 0 / 8 across the three runs, `token_lane_mismatch` 0 everywhere; `final_generation` is `[9, 9, 9, 9]`, `[7, 7, 7, 7]` and `[9, 9, 9, 9]` — one bump per death per lane.

Artifacts: [`docs/evidence/2026-10-30_pool_stress_report.json`](docs/evidence/2026-10-30_pool_stress_report.json), [`docs/evidence/2026-10-30_pool_lease_stress_report.json`](docs/evidence/2026-10-30_pool_lease_stress_report.json), [`docs/evidence/2026-10-30_pool_mixed_stress_report.json`](docs/evidence/2026-10-30_pool_mixed_stress_report.json); they supersede the 2026-10-26 set. The same three commands run on every push in CI (`.github/workflows/ci.yml`, the `Release ignored lanes (ubuntu)` job, `cargo test --release --locked --test supervisor_pool_stress{,_lease,_mixed} -- --ignored`); that job reports its own verdict and never rewrites the committed rows, so a CI-green pool lane and a doc row can disagree only on `wall_ms`.

```bash
cargo test --release --test supervisor_pool_stress -- --ignored --nocapture
cargo test --release --test supervisor_pool_stress_lease -- --ignored --nocapture
cargo test --release --test supervisor_pool_stress_mixed -- --ignored --nocapture
```
