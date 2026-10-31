# Committed storm and bench artifacts

Storm reports are written into `target/` (the gitignored build tree), so every
re-run overwrites the previous evidence and no number quoted in the docs could
be re-derived later. The files here are verbatim copies of what the harness
printed, one per published figure. Nothing in this directory is edited after
the run: when a run is refreshed, a new dated file is added and the old one
stays as the record of the tree that produced it.

| Artifact | Command that produced it | Range of interest |
|---|---|---|
| `2026-10-31_multi_lineage_armedhold_report.json` + `_run.log` | `powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive -ArmedHold -ReportPath target/pub_armedhold.json -StatsPath target/pub_armedhold_stats.json -LogPath target/pub_armedhold.log -LaneRoot target/pub_armedhold` | `verdict`, `report_schema: 2`, wall 25.82 s, 387.2/453.3 kills/s, dispatch p50/p99/max, the nine `band_*` counts, per-lane `resources`, `killers_saw`, `system`, 48 gates |
| `2026-10-31_multi_lineage_freerun_report.json` + `_run.log` | same command without `-ArmedHold` and with the `pub_freerun*` paths | ambush 1,999 / escape 77 / stillborn 7,924, wall 14.13 s, 707.6 kills/s, 43 gates |
| `2026-10-31_multi_lineage_saturation_report.json` + `_run.log` | same command at `-Lines 8 -KillsPerLine 1250`, `pub_saturation*` paths | 8-lane dispatch p99 5,024.2 µs against the 4-lane 1,360.6 µs, 92 gates |
| `2026-10-30_pool_stress_report.json` | `cargo test --release --test supervisor_pool_stress -- --ignored --nocapture` | `reaped`/`ambush` 32/32, `tokens_seen`, `final_generation`, `wall_ms` |
| `2026-10-30_pool_lease_stress_report.json` | `cargo test --release --test supervisor_pool_stress_lease -- --ignored --nocapture` | `stillborn` 24/24, `lease_kills` 24, `monitor_checks` 102,471 |
| `2026-10-30_pool_mixed_stress_report.json` | `cargo test --release --test supervisor_pool_stress_mixed -- --ignored --nocapture` | `ambush`/`escape`/`stillborn`/`deadlocks` = 8/8/8/8, `deadlock: true` |
| `2026-10-30_bench_snapshot.md` | `cargo bench --locked --bench admission_bench` (all groups, second of two full runs), table assembled by `bash scripts/bench.sh` | every `**<num> ns/us**` row cited by BENCHMARKS.md |
| `2026-10-30_multi_lineage_{armedhold,freerun,saturation}_report.json` + `_run.log` | the same three commands with the `storm_*` paths | superseded by the 2026-10-31 set; kept as the record of the 44 / 39 / 84-gate battery that had no per-lane `armed_hold matches flag` shape gate |
| `2026-10-27_multi_lineage_{armedhold,freerun}_report.json` + `_stats.json` | `powershell -ExecutionPolicy Bypass -File scripts/stress_multi_lineage.ps1 -Lines 4 -KillsPerLine 2500 -Aggressive [-ArmedHold]` | superseded by the 2026-10-31 set; kept as the record of that tree and of the `le_*` histogram schema |
| `2026-10-26_pool_*_stress_report.json` | `cargo test --test supervisor_pool_stress{,_mixed,_lease} -- --ignored --nocapture` (debug profile) | superseded by the 2026-10-30 pool set |
| `2026-10-26_bench_snapshot.md` | `cargo bench` (all groups), table assembled by `bash scripts/bench.sh` | superseded by `2026-10-30_bench_snapshot.md` |

## What the schema carries

`report_schema: 2` is the harness declaring which keys it wrote. Schema 1 named
the histogram buckets `le_50` … `gt_10000` while storing the count of the band
`(prev, bucket]`: a disjoint count under a cumulative name, so `<=500: 7272`
read as "7,272 dispatches under 500 µs" when the run had 8,942 of them. Schema
2 renames them to `band_50` … `band_above_10000`, and the sum of the nine bands
equals `kills_completed` — which is now one of the assertions. A changed report
shape must bump the number here and in `scripts/stress_multi_lineage.{ps1,py}`
together; `test_storm_harnesses_publish_one_report_schema` refuses the two
harnesses drifting apart, so one oracle can read a Windows and a POSIX artifact.

Schema 2 also adds the `system` block (`cpu_name`, physical/logical cores, OS
caption and build, `rustc`, PowerShell version, build profile) so a published
latency is attributed to the host that measured it, and `resources.sample_drops`
so a window that could not be read is visible rather than folded into the
growth arithmetic. Each run writes a `_run.log` console transcript next to its
report: it carries the lane-by-line launch lines, the per-check results and the
verdict banner, which is what a CI failure needs. The committed transcripts are
verbatim except for identity: the only absolute paths in them sit under the
repository root (`G:\forder\snubber\target\…`), and they contain no account
name and no profile directory.

On a runner the same `checks[]` table also goes where a reader without a GitHub
session can reach it: `scripts/ci_storm_summary.py` runs as a step after every
storm in `.github/workflows/stress.yml` and turns each failed or skipped gate
into a workflow annotation, which the anonymous
`/check-runs/<id>/annotations` API returns — the step log and the artifact zip
of a public repository do not. It reads the transcript too, so a storm that
exited before writing a report still publishes the stage it stopped at, and a
storm that never started says "not attempted" instead of looking like a crash.
It always exits 0: the storm's own step owns the verdict.

Before the first storm, every `stress.yml` flavor runs two gates on the harness
itself: `python -m py_compile scripts/*.py` proves each script parses, and
`scripts/check_python_bindings.py` proves no scope reads a name it never binds
(`--self-test` first proves that checker still reports a planted unbound read).
The second gate exists because the POSIX storms died twice on exactly that
defect -- a `NameError` raised after the supervisors were spawned, so the run
left live processes and no report while the compile gate stayed green.

Every lane also gates the shape it ran. The worker stamps `armed_hold` into its
own report and the harness compares that stamp with the flag it actually passed:
`lane N armed_hold matches flag` in PowerShell, `lane N armed_hold matches` in
Python. The POSIX harness has had this gate since schema 2; the PowerShell one
gained it for the 2026-10-31 set, which is why its per-lane battery is 11 gates
under `-ArmedHold` and 10 without it rather than the 10 and 9 of 2026-10-30.
Without it a lane that silently lost the flag would still publish a PASS under
the armed-hold label, and armed-hold is the row BENCHMARKS.md and the CI
annotations quote. `test_storm_tables_match_committed_reports` refuses an
artifact whose gate count does not match `lanes x per_lane + aggregate`, and
`test_both_storm_harnesses_gate_the_armed_hold_shape` refuses either harness
dropping the gate.

The raw per-lane dispatch series (`-StatsPath` / `--stats`) is deliberately not
committed for the 2026-10-30 and 2026-10-31 runs: it is ~10,000 wall-clock
samples per report, and BENCHMARKS.md quotes no number that the report does not
already carry. The
two 2026-10-27 `_stats.json` files stay committed as the byte-identical
duplicates of their reports that they are — the harness of that tree wrote the
same payload to both paths, which is one of the reasons it was split.

## How these files are read

`tests/doc_sync.rs` is the consumer, and it reads with a scoped JSON reader
(one key inside one object) rather than a first-occurrence search, because
`dispatch_us`, `samples`, `p50` and `final_generation` each appear once in the
aggregate and once per lane: an unscoped reader answers an aggregate question
with lane 0's number and never says so.

- `test_storm_tables_match_committed_reports` — every storm row, per-lane
  bullet, band line, killer-counter line, machine caption and rescue-accounting
  fragment is re-derived from the report at test time and must match the doc
  line byte for byte. The artifact is checked against itself first (verdict,
  schema, release profile, gate count, conservation across lanes,
  `final_generation == kills + 1`, zero stale pids,
  `tokens_rescued + clean_settles == kills_completed`), because a doc quoting a
  self-inconsistent report is broken twice over.
- `test_pool_storm_tables_match_committed_reports` — the same for the three
  pool rows. It also reads `docs/USER_GUIDE.md`, because the guide is prose
  about these runs: it must name the committed set and must not copy a clock
  column out of it.
- `test_bench_numbers_match_evidence_artifact` and
  `test_bench_rows_quote_their_own_measurement` — the bench rows against
  `2026-10-30_bench_snapshot.md`. The snapshot path is one constant,
  `BENCH_ARTIFACT`, and BENCHMARKS.md must link the file that constant names.

Both directions matter: refresh the artifact by re-running the storm, then
regenerate the doc from it. Never edit a report to match a sentence.

`wall`, `kills/s`, dispatch percentiles, `monitor_checks`, `sample_drops` and
`wall_ms` are wall-clock observations — they move with load, compiler and
scheduler, so a re-run on the same tree is allowed to differ. The structural
counters (`kills_completed`, the band counts, `true_ambush` / `clean_escape` /
`stillborn`, `tokens_rescued`, `clean_settles`, `stale`, `final_generation`,
`conservation_breaks`) are deterministic, and the gates that compare lanes to
the aggregate hold on every run. A fresh run may overwrite
`target/multi_lineage_report.json` and `target/pool_*_report.json` but never
these copies.
