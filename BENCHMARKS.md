# Benchmarks

The single home for measured latency figures in the public docs. This file
carries the full operation matrix and the contended group; README.md and
docs/USER_GUIDE.md link here and quote no measured numbers. The API & cost
quick reference — signatures, layers and footprints, no measurements —
lives at the end of docs/USER_GUIDE.md. `tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact`
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
[`max + 1`](docs/USER_GUIDE.md#layer-2-concurrency-bulkhead-bounded-queues-backpressure)
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
