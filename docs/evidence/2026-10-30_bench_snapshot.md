# Committed bench snapshot (2026-10-30)

Frozen copy of `target/bench/snapshot.md` — the per-operation mean point
estimates (ns/op) that `scripts/bench.sh` assembles from the Criterion
summaries. BENCHMARKS.md — the single numeric home of the public docs —
cites these numbers and
`tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact` checks
every `~<number>` latency citation against this file, so the table below
is committed verbatim.

Machine: AMD Ryzen 5 4600H, Windows 10 x64, Criterion release profile.
Run: `cargo bench --locked --bench admission_bench` — every bench group,
64 rows, run in full twice on 2026-10-30; this file is the second run.
The first run agreed row-for-row outside the contended group: median
absolute delta 0.8 %, and the movers were `gateway send (6 threads)`
(192.9 → 217.2 ns) and `IsrBridge::try_push + try_pop (2 threads)`
(34.0 → 31.2 ns), which move between runs by design. A fresh run
overwrites `target/bench/snapshot.md`; regenerate it from stored Criterion
results with `bash scripts/bench.sh --skip-run`. This copy is never
overwritten.

Supersedes `2026-10-26_bench_snapshot.md`, kept as the evidence of what
the earlier tree measured. It adds two bench groups —
`layer2_watchdogs` and `layer5_cold_evidence` — which is why the table is
64 rows where the old one was 53.

Units: `us` is microseconds, everything else nanoseconds. Rows whose doc
counterparts are derived per-frame/per-lane figures (the `drain_sync` and
`feed` frame costs, the per-lane share of `pool.expired_lanes`) are the
same measurement divided by the row's stated frame/lane count; the doc
test skips those tokens by rule.

| Operation | Average Latency | Notes |
|---|---|---|
| `enter_erased_holding_auto + commit (6 threads)` | **1.25 us** | contended_multithreaded bench |
| `gateway send (6 threads)` | **217.2 ns** | contended_multithreaded bench |
| `node.enter + commit (6 threads)` | **1.03 us** | contended_multithreaded bench |
| `node.scoped (6 threads)` | **1.04 us** | contended_multithreaded bench |
| `IsrBridge::try_push + try_pop (2 threads)` | **31.2 ns** | contended_multithreaded bench |
| `classify_stall_ceiling` | **1.9 ns** | layer1_5_hang_isr bench |
| `IsrBridge::try_push + try_pop` | **7.1 ns** | layer1_5_hang_isr bench |
| `IsrBridge::trip + take_trips` | **9.4 ns** | layer1_5_hang_isr bench |
| `WorkerPulse::kick + PulseWatch::observe` | **5.8 ns** | layer1_5_hang_isr bench |
| `EpochGate::observe` | **2.6 ns** | layer1_core bench |
| `SnubberGuard::new + commit` | **5.3 ns** | layer1_core bench |
| `enter_async_auto (ready poll)` | **151.4 ns** | layer2_async_waiters bench |
| `enter_async_auto park/wake/admit` | **474.0 ns** | layer2_async_waiters bench |
| `ConflateSender::publish (occupied slot)` | **15.6 ns** | layer2_impedance bench |
| `ConflateSender::publish + take_latest` | **21.4 ns** | layer2_impedance bench |
| `guard.commit_enveloped(64 B)` | **116.6 ns** | layer2_impedance bench |
| `node.enter_auto + commit` | **95.6 ns** | layer2_std bench |
| `node.enter + commit` | **90.9 ns** | layer2_std bench |
| `node.enter_with_priority(Critical)` | **92.8 ns** | layer2_std bench |
| `node_passthrough.enter + commit` | **55.8 ns** | layer2_std bench |
| `node_passthrough.scoped + commit` | **50.8 ns** | layer2_std bench |
| `node.scoped (work + auto-commit)` | **90.4 ns** | layer2_std bench |
| `node.transition` | **93.4 ns** | layer2_std bench |
| `TaskConduit::check_directive` | **1.0 ns** | layer2_telemetry bench |
| `gateway send -> recv roundtrip` | **50.1 ns** | layer2_telemetry bench |
| `gateway send (HWM/LWM throttled)` | **16.6 ns** | layer2_telemetry bench |
| `node.next_sequence (auto ticket)` | **4.8 ns** | layer2_telemetry bench |
| `node.stats() snapshot` | **58.0 ns** | layer2_telemetry bench |
| `SnubberStats::shed_total + success_rate_per_mille` | **29.0 ns** | layer2_telemetry bench |
| `AimdLimit::on_success + on_pressure` | **3.7 ns** | layer2_watchdogs bench |
| `HandleWatch<10>::sample (probe)` | **17.8 ns** | layer2_watchdogs bench |
| `jitter_ms (20 % spread)` | **10.5 ns** | layer2_watchdogs bench |
| `MemoryTrend<10>::sample` | **9.6 ns** | layer2_watchdogs bench |
| `MemoryTrend<10>::verdict` | **13.9 ns** | layer2_watchdogs bench |
| `probe_erased_auto + commit` | **168.2 ns** | layer3_alloc bench |
| `PipeDrain::feed (256 frames)` | **10.82 us** | layer4_drain_feed bench |
| `PipeDrain::feed (one frame)` | **97.3 ns** | layer4_drain_feed bench |
| `friction_fn handler call (Arc<dyn Fn>)` | **1.8 ns** | layer4_extensibility bench |
| `friction_wait_ms (adaptive, Arc handler)` | **21.5 ns** | layer4_extensibility bench |
| `friction_wait_ms (adaptive, fn)` | **21.7 ns** | layer4_extensibility bench |
| `NodeName::as_str` | **1.0 ns** | layer4_extensibility bench |
| `NodeName::Owned clone` | **9.9 ns** | layer4_extensibility bench |
| `NodeName::Static clone` | **1.5 ns** | layer4_extensibility bench |
| `ProcessToken::from_bytes` | **7.2 ns** | layer4_layer5_diagnostics bench |
| `process_token.to_bytes()` | **7.6 ns** | layer4_layer5_diagnostics bench |
| `snapshot_v1.encode()` | **1.12 us** | layer4_layer5_diagnostics bench |
| `state_digest.crc()` | **261.7 ns** | layer4_layer5_diagnostics bench |
| `static_node.enter + commit` | **89.8 ns** | layer4_layer5_diagnostics bench |
| `PipeDrain::drain_sync (256 frames)` | **11.04 us** | layer4_pipe_drain bench |
| `StateDigest::write_to(&mut sink)` | **610.9 ns** | layer4_token_sink bench |
| `ProcessToken::write_to(&mut sink)` | **2.0 ns** | layer4_token_sink bench |
| `pool.expired_lanes(lease) (4 lanes)` | **516.2 ns** | layer4_worker_pool bench |
| `lane.attach + note_death (ambush) -> PoolReap` | **197.8 ns** | layer4_worker_pool bench |
| `lane.attach + note_death (escape) -> PoolReap` | **186.1 ns** | layer4_worker_pool bench |
| `namespace_task::pack` | **1.0 ns** | layer4_worker_pool bench |
| `namespace_task::unpack (task_lane + task_local)` | **1.0 ns** | layer4_worker_pool bench |
| `lane.note_memory_kb` | **11.8 ns** | layer4_worker_pool bench |
| `pool.report() (4 lanes)` | **107.1 ns** | layer4_worker_pool bench |
| `FlightTape<6>::last_with_code` | **3.3 ns** | layer5_cold_evidence bench |
| `FlightTape<6>::record_code + newest` | **5.1 ns** | layer5_cold_evidence bench |
| `RankedSet::insert (teardown order)` | **24.3 ns** | layer5_cold_evidence bench |
| `snapshot_v1.decode()` | **1.12 us** | layer5_cold_evidence bench |
| `snapshot_v1.diff (counter deltas)` | **22.9 ns** | layer5_cold_evidence bench |
| `write_triage(card)` | **629.8 ns** | layer5_cold_evidence bench |
