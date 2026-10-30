# Committed storm artifacts

`tests/supervisor_pool_stress*.rs` write their JSON report into `target/`
(the repository's gitignored build tree), so every re-run overwrites the
previous evidence and no number quoted in the docs can be re-derived later.
The files here are frozen snapshots of those reports, copied verbatim.

Machine: AMD Ryzen 5 4600H, Windows 10 x64, debug profile (`cargo test`,
no `--release`), 2026-10-26.

| Artifact | Command | Range of interest |
|---|---|---|
| `2026-10-26_pool_stress_report.json` | `cargo test --test supervisor_pool_stress -- --ignored --nocapture` | `reaped`/`ambush` 32/32, `wall_ms` |
| `2026-10-26_pool_mixed_stress_report.json` | `cargo test --test supervisor_pool_stress_mixed -- --ignored --nocapture` | `ambush`/`escape`/`stillborn`/`deadlocks` = 8/8/8/8, `deadlock=true` |
| `2026-10-26_pool_lease_stress_report.json` | `cargo test --test supervisor_pool_stress_lease -- --ignored --nocapture` | `stillborn` 24/24, `lease_kills=24`, `monitor_checks` |
| `2026-10-26_bench_snapshot.md` | `cargo bench` (all groups), table assembled by `bash scripts/bench.sh` | superseded by `2026-10-30_bench_snapshot.md`; kept as the record of what that tree measured |
| `2026-10-30_bench_snapshot.md` | `cargo bench --locked --bench admission_bench` (all groups, second of two full runs), table assembled by `bash scripts/bench.sh` | every `**<num> ns/us**` row cited by BENCHMARKS.md |
| `2026-10-27_multi_lineage_armedhold_report.json` + `_stats.json` | `powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold` | ambush 10,000/10,000, wall 23.96 s, dispatch p50 323.4 µs |
| `2026-10-27_multi_lineage_freerun_report.json` + `_stats.json` | same command without `-ArmedHold` | ambush 1,800 / escape 67 / stillborn 8,133, wall 14.48 s |

The multi-lineage storm artifacts are verbatim copies of
`target/multi_lineage_report.json` and `target/multi_lineage_stats.json`
as the PowerShell harness writes them (UTF-8 with BOM). Machine: AMD
Ryzen 5 4600H, Windows 10 x64, release profile, 2026-10-27. The storm
tables in BENCHMARKS.md quote these files; the CI runner rows in the
same section quote CI logs and are not repository artifacts.

`2026-10-30_bench_snapshot.md` (and the 2026-10-26 copy it supersedes) is a
different kind of artifact: it comes from `cargo bench` (release profile,
Criterion), not from a test-written report, and its rows are the latencies
cited in BENCHMARKS.md (the single numeric home; README.md and
docs/USER_GUIDE.md link to it). Two gates read it:
`tests/doc_sync.rs::test_bench_numbers_match_evidence_artifact` checks
every `~<number>` latency citation in BENCHMARKS.md against it, and
`test_bench_rows_quote_their_own_measurement` checks that each row quotes
the measurement of its OWN row — the existence check alone let
`pool.expired_lanes` carry another row's number while the suite stayed
green. The doc links exactly one snapshot file and `BENCH_ARTIFACT` in
`tests/doc_sync.rs` names the same constant, so a refreshed snapshot
cannot be pointed at by the reader and missed by the gate.

`wall_ms` and `monitor_checks` are wall-clock observations: they move with
load, compiler and scheduler. The structural counters (`reaped`, `ambush`,
`escape`, `stillborn`, `deadlocks`, `lease_kills`, `conservation_breaks`,
`final_generation`) are deterministic. A fresh run may overwrite
`target/pool_*_report.json` but never these copies.
