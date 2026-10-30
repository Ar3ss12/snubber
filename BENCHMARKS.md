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

`scripts/stress_multi_lineage.ps1` runs N supervisors in parallel, each with its own worker, pid file, pipe and raw Win32 killer. One supervisor = one lineage: isolation needs no library changes because each lane runs in its own working directory. `--armed-hold` parks the worker armed and publishes its pid only after the arm frame, so an ambush-rate gate cannot be satisfied by shooting processes that never armed. `scripts/stress_multi_lineage.py` is the same harness for Linux and macOS: `SIGKILL` instead of `TerminateProcess`, process identity from `/proc/<pid>/stat` starttime (Linux) or `ps -o lstart` (macOS), so a recycled pid is never mistaken for the live worker. Python 3 standard library only, no dependencies.

Both runs below are the 2026-10-27 Win32 storms on AMD Ryzen 5 4600H, release build, 4 lanes × 2,500 = 10,000 `TerminateProcess` each, quoted from the committed reports [`docs/evidence/2026-10-27_multi_lineage_armedhold_report.json`](docs/evidence/2026-10-27_multi_lineage_armedhold_report.json) and [`docs/evidence/2026-10-27_multi_lineage_freerun_report.json`](docs/evidence/2026-10-27_multi_lineage_freerun_report.json) (raw JSON copies, verdict `PASS` on every per-lane and aggregate gate):

| Mode | Wall | Kills/s (avg / peak) | Dispatch p50 | Ambush | Escape | Stillborn |
|---|---|---|---|---|---|---|
| `-Aggressive -ArmedHold` | 23.96 s | 417.4 / 462.2 | 323 µs | 10,000 (100%) | 0 | 0 |
| `-Aggressive` (free-running) | 14.48 s | 690.7 / 822.0 | 419 µs | 1,800 (18.0%) | 67 (0.7%) | 8,133 (81.3%) |

Armed-hold run: per-lane dispatch p50 313.9–328.0 µs, private-memory growth +0.93…+1.08 MB, handle growth −3…+6, final generation = kills + 1 (2501) on every lane, no pipe deadlock. Free-running shows what the rate costs: at ~691 kills/s most shots land during process startup, not in the armed window (per-lane p50 415.6–421.0 µs, growth +0.70…+1.01 MB, handles 0…+5, generation 2501). The bucket counters and generations are deterministic; `wall`, `kills/s` and dispatch percentiles are wall-clock observations that move with load.

The same gates run in CI (`.github/workflows/stress.yml`, `--aggressive`, dispatch latency = `SIGKILL`/`TerminateProcess` call only; runner-local CI-log figures, not repository artifacts):

| Runner | Mode | Kills | Wall | Kills/s | Ambush | Escape | Stillborn | p50 / p99 |
|---|---|---|---|---|---|---|---|---|
| ubuntu-latest (2 vCPU) | armed-hold | 1,000 | 0.91 s | 1,095 | 100% | 0 | 0 | 5 / 11–14 µs |
| ubuntu-latest (2 vCPU) | free-running | 2,000 | 1.11 s | 1,803 | 93.2% | 2.4% | 4.5% | 4 / 12–17 µs |
| macos-latest (arm64) | armed-hold | 1,000 | 7.19 s | 139.1 | 100% | 0 | 0 | 22–24 / 344–554 µs |
| macos-latest (arm64) | free-running | 2,000 | 31.82 s | 62.9 | 61.9% | 38.1% | 0 | 13–14 / 57–84 µs |

```powershell
# Single-lineage release gate (10,000 kills)
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 10000 -Launch -RawKill -Aggressive
# Armed-hold rescue proof (true ambush >= 70%)
powershell -ExecutionPolicy Bypass -File scripts/stress_harness.ps1 -Kills 1000 -Launch -RawKill -Aggressive -ArmedHold
# 4 independent lineages, 2,500 kills each
powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold
```

```bash
# Linux / macOS: same storm, SIGKILL path, JSON gates
python3 scripts/stress_multi_lineage.py --lines 4 --kills-per-line 2500 --aggressive --armed-hold
```
