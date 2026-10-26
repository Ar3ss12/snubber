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

`wall_ms` and `monitor_checks` are wall-clock observations: they move with
load, compiler and scheduler. The structural counters (`reaped`, `ambush`,
`escape`, `stillborn`, `deadlocks`, `lease_kills`, `conservation_breaks`,
`final_generation`) are deterministic. A fresh run may overwrite
`target/pool_*_report.json` but never these copies.
