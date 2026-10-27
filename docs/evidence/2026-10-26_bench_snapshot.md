# Committed bench snapshot (2026-10-26)

Frozen copy of `target/bench/snapshot.md` — the per-operation mean point
estimates (ns/op) that `scripts/bench.sh` assembles from the Criterion
summaries. README.md and docs/USER_GUIDE.md cite these numbers and
`tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact` checks
every `~<number>` latency citation against this file, so the table below
is committed verbatim.

Machine: AMD Ryzen 5 4600H, Windows 10 x64, Criterion release profile.
Run: `cargo bench` (all bench groups), 2026-10-26. A fresh run overwrites
`target/bench/snapshot.md`; regenerate it from stored Criterion results
with `bash scripts/bench.sh --skip-run`. This copy is never overwritten.

Units: `us` is microseconds, everything else nanoseconds. Rows whose doc
counterparts are derived per-frame/per-lane figures (`~44.5 ns/frame`,
`~130 ns per lane`) are the same measurement divided by the row's stated
frame/lane count; the doc test skips those tokens by rule.

| Operation | Average Latency | Notes |
|---|---|---|
| `gateway send (6 threads)` | **183.7 ns** | contended_multithreaded bench |
| `node.enter + commit (6 threads)` | **971.2 ns** | contended_multithreaded bench |
| `node.scoped (6 threads)` | **923.0 ns** | contended_multithreaded bench |
| `IsrBridge push/pop (2 threads)` | **40.0 ns** | contended_multithreaded bench |
| `classify_stall_ceiling` | **2.0 ns** | layer1_5_hang_isr bench |
| `IsrBridge::try_push + try_pop` | **7.1 ns** | layer1_5_hang_isr bench |
| `IsrBridge::trip + take_trips` | **9.3 ns** | layer1_5_hang_isr bench |
| `WorkerPulse::kick + PulseWatch::observe` | **5.8 ns** | layer1_5_hang_isr bench |
| `EpochGate::observe` | **2.6 ns** | layer1_core bench |
| `SnubberGuard::new + commit` | **5.4 ns** | layer1_core bench |
| `enter_async_auto (ready poll)` | **156.0 ns** | layer2_async_waiters bench |
| `enter_async_auto park/wake/admit` | **542.2 ns** | layer2_async_waiters bench |
| `ConflateSender::publish (occupied slot)` | **15.7 ns** | layer2_impedance bench |
| `ConflateSender::publish + take_latest` | **21.7 ns** | layer2_impedance bench |
| `guard.commit_enveloped(64 B)` | **142.5 ns** | layer2_impedance bench |
| `node.enter_auto + commit` | **93.3 ns** | layer2_std bench |
| `node.enter + commit` | **90.5 ns** | layer2_std bench |
| `node.enter_with_priority(Critical)` | **86.7 ns** | layer2_std bench |
| `node_passthrough.enter + commit` | **55.2 ns** | layer2_std bench |
| `node_passthrough.scoped + commit` | **51.2 ns** | layer2_std bench |
| `node.scoped (work + auto-commit)` | **88.0 ns** | layer2_std bench |
| `node.transition` | **90.3 ns** | layer2_std bench |
| `TaskConduit::check_directive` | **1.0 ns** | layer2_telemetry bench |
| `gateway send -> recv roundtrip` | **50.8 ns** | layer2_telemetry bench |
| `gateway send (HWM/LWM throttled)` | **16.8 ns** | layer2_telemetry bench |
| `node.next_sequence (auto ticket)` | **4.8 ns** | layer2_telemetry bench |
| `node.stats() snapshot` | **58.4 ns** | layer2_telemetry bench |
| `SnubberStats::shed_total + success_rate_per_mille` | **28.4 ns** | layer2_telemetry bench |
| `enter_erased_auto` | **205.4 ns** | layer3_alloc bench |
| `PipeDrain::feed (256 frames)` | **10.74 us** | layer4_drain_feed bench |
| `PipeDrain::feed (one frame)` | **98.5 ns** | layer4_drain_feed bench |
| `friction_fn handler call (Arc<dyn Fn>)` | **1.8 ns** | layer4_extensibility bench |
| `friction_wait_ms (adaptive, Arc handler)` | **21.7 ns** | layer4_extensibility bench |
| `friction_wait_ms (adaptive, fn)` | **21.8 ns** | layer4_extensibility bench |
| `NodeName::as_str` | **1.0 ns** | layer4_extensibility bench |
| `NodeName::Owned clone` | **10.0 ns** | layer4_extensibility bench |
| `NodeName::Static clone` | **1.5 ns** | layer4_extensibility bench |
| `ProcessToken::from_bytes` | **7.4 ns** | layer4_layer5_diagnostics bench |
| `process_token.to_bytes()` | **7.9 ns** | layer4_layer5_diagnostics bench |
| `snapshot_v1.encode()` | **1.11 us** | layer4_layer5_diagnostics bench |
| `state_digest.crc()` | **266.1 ns** | layer4_layer5_diagnostics bench |
| `static_node.enter + commit` | **125.1 ns** | layer4_layer5_diagnostics bench |
| `PipeDrain::drain_sync (256 frames)` | **11.40 us** | layer4_pipe_drain bench |
| `StateDigest::write_to(&mut sink)` | **615.0 ns** | layer4_token_sink bench |
| `ProcessToken::write_to(&mut sink)` | **1.8 ns** | layer4_token_sink bench |
| `pool.expired_lanes(lease) (4 lanes)` | **519.4 ns** | layer4_worker_pool bench |
| `lane.attach + note_death (ambush) -> PoolReap` | **225.3 ns** | layer4_worker_pool bench |
| `lane.attach + note_death (escape) -> PoolReap` | **186.9 ns** | layer4_worker_pool bench |
| `namespace_task::pack` | **1.0 ns** | layer4_worker_pool bench |
| `namespace_task::unpack` | **1.0 ns** | layer4_worker_pool bench |
| `lane.note_memory_kb` | **11.9 ns** | layer4_worker_pool bench |
| `pool.report() (4 lanes)` | **106.9 ns** | layer4_worker_pool bench |
