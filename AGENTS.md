# AGENTS.md — snubber (zero-dep RAII damper + bulkhead node)

Operating rules for AI agents working in this repo. They exist because this
project already survived three classic traps and a fake-green torture test —
all documented here so the next agent does not re-invent them.

## 0. First rule: gates before theories

Run this FIRST, before touching a line:

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo check --no-default-features
cargo test --release --test cycle_torture
```

`cargo test` today = **168 passed** (7 unit + 161 integration in 42 files)
+ 20 doc-tests = **188 green**. If your change moves a documented number,
update it in the SAME commit (README + tech.md). Stale numbers send the
next agent hunting ghosts.

Windows note: `target/` incremental cache wedges with `os error 32`.
If the build complains about `dep-graph.bin` / missing CGU object —
`rmdir /s /q target` and rebuild clean. That is the environment, not code.

## 1. ZERO-PANIC phases A/B/C (load-bearing, not cosmetic)

`SnubberNode::enter_inner` (`src/node.rs`) is split in three:

```text
PHASE A: snapshot (lock, copy primitives, drop)
PHASE B: compute   (NO lock — format!, clone(), friction_fn, hooks)
PHASE C: commit    (lock, saturating writes, drop)
```

Under the lock ONLY: `Copy` moves, `saturating_add/sub`, flag flips, one
`Arc` refcount bump (`hooks: Arc<Vec<..>>`). FORBIDDEN under the lock:
`format!`, `String`, `Vec::clone`, `stats.clone()` (owns `last_fault`),
`record.clone()` (owns `FaultDetail::Owned`), `friction_fn`, hooks,
`custom`, any closure parameter (`record_locked` takes `(FaultKind, u8, u64)`
codes precisely so a panicking closure cannot be smuggled in).

Why: if nothing under the lock can panic, the mutex is physically
unpoisonable. `poison_recoveries` goal is 0.

## 2. The breaker OFF trap ("1 of 40000")

`trip_after_failures(0)` disables the STREAK (`trips > 0 && ...`, never
fires) — but the POLICY `Panic -> TripBreaker` (default table) still trips
OPEN on the first `report(Panic)`. Torture that reports Panic 416 times
with default policy isolates ITSELF after report #1: `started=1`,
`breaker=39999`. Fix (already in `record_locked`): policy trip honors
`trip_after_failures(0) + failure_window(0)` = fully OFF. Torture nodes
additionally override Panic to `DiscardSilently`; trip behavior is covered
by `tests/reflex_breaker.rs` + `tests/node_breaker.rs`, not by throughput
tests. Never assume `0 = OFF` for the whole breaker — check streak AND
policy AND window.

One armed trigger at a time: `half_open_probe` admits exactly one probe;
no second probe in parallel, no manual `breaker = HalfOpen` outside
`record_locked` / `note_outcome` / `reset_breaker`. Tripwire:
`tests/reflex_breaker.rs` two waves (1 probe → 8 flood).

## 3. EpochGate is single-lane — do not bench it with 8 lanes

`EpochGate` is monotonic per PRODUCER (one clicker, one laser, epochs
1,2,3...). A global `AtomicU64` allocator shared by 8 threads hands e=11
to a fast thread before e=10 reaches the gate — the gate CORRECTLY cuts
e=10 as stale. That is the gate doing its job, not a bug.

Throughput tests therefore: global allocator + bounded stale-retry
(cap 64) with `STALE_HITS` counted separately, conservation as
`attempts == started + bulk + stale + brk + rate` AND
`started == THREADS * PER_THREAD`. A retry loop that hides `StaleEpoch`
without counting it is cardboard. `stale≈200/40k` debug noise is honest;
`stale=34992` with disjoint lanes was the test being wrong.

F3 CONTRACT (library stays ruthless): the retry loop above lives ONLY in
the test harness (`tests/cycle_torture.rs`), which imitates a parallel
storm. Inside `snubber` itself `StaleEpoch` is a HARD refusal — `enter`
returns `Err`, the gate does not advance, no hidden re-issue. NEVER move
the retry inside the library: a hidden in-crate retry turns a fail-fast
membrane into a parking lot and buries backpressure telemetry.

## 4. Green tests are guilty until proven with counters

The "Potemkin" lesson (`1 = 1 + 0 + 0`): `assert_eq!(started, settled)`
alone proves NOTHING if 39999 attempts bounced off the door. Every
torture/stress test MUST assert:
- `attempts == started + bulk + stale + breaker + rate` (none vanish),
- `started >= EXPECTED` hard floor (e.g. `started == 40000`),
- per-kind test counters == node books (`bulk == bulkhead_rejections`),
- `rollbacks_fired == aborts + drops` (REAL `fn rollback`, never `|| {}`),
- `poison_recoveries == 0`.

`tests/cycle_torture.rs` (CAMP 1, reflexes OFF) + `tests/reflex_breaker.rs`
+ `tests/reflex_bulkhead.rs` (CAMP 2, reflexes ON) are the reference shape.
`input_boundary.rs::hotkey_flap` (`admitted == 100` + `stale == 100`) is the
small-scale shape. Copy them, do not invent quieter ones.

## 5. Invariants: hard math, never debug_assert

Slot ceilings: `hard_clamp_slots()` + `saturating_*` everywhere — active
in `--release`. `debug_assert` evaporates in release and protects nothing.
Every counter bump in `src/node.rs` is `saturating_add/sub`. Keep it that
way; `+=` on a counter is a review-blocking defect.

## 6. Open-call + unwind

Hooks / custom / fallback / `friction_fn` / console run OUTSIDE the lock
(`emit`, PHASE B). A hook may call `stats()` re-entrantly. `isolate_once`
gives each foreign call its own panic boundary — one rotten observer never
beheads the chain. A panic in PHASE B unwinds with books settled; the
caller's `Drop` (outer meta-guard) fires during unwinding — hardware is
released. Direction one-way only: dumb guards smart, never node-in-node.

Settle order (reverse of arm, load-bearing): hardware FIRST
(`guard.take()` + `drop` → rollback runs), books SECOND
(`in_flight`/`queued`/`stats` via `saturating_*`), lock drop THIRD,
`emit` LAST with no lock held. Any `emit` before `drop(inner)` is a
review-blocking defect — it smuggles foreign code under the mutex
(F11). The order already holds in `commit` / `abort` / `settle_implicit`;
this paragraph exists so the next refactor does not re-order it.

## 7. Docs policy

Public: `README.md` only (+ `LICENSE` if added). `AGENTS.md`, `tech.md`,
`docs/` are INTERNAL working docs. Numbers come from real runs.
New sections: English from the start; quoted logs/CLI stay byte-exact.


## 8. Gates table — copy commands verbatim (Windows)

`cmd.exe` does NOT understand `#` comments or `|` the way PowerShell
does. A trailing `#` comment after `cargo test` silently filters the
suite (nanoclick precedent: `11 passed; 173 filtered out`). After
`cargo clean` build single-streamed — parallel rebuilds wedge the
incremental cache (`os error 32` here, `os error 1455` there):

| Gate | Command (verbatim) | Green today |
|---|---|---|
| fmt | `cargo fmt --check` | clean |
| lint | `cargo clippy --all-targets -- -D warnings` | clean |
| unit+integration | `cargo test` | 168 + 20 doc = 188 green |
| features | `cargo check --no-default-features` | ok |
| miri (lib UB check) | `cargo +nightly miri test --lib` | 6 unit tests ok (0 UB) |
| miri (integration suite) | `$env:MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks"; cargo +nightly miri test --test guard --test scoped_ergonomics --test erased --test closure_policy --test invariant --test watchdog --test memory_boundary --test presets --test process_died` | 36 integration tests ok (0 UB) |
| miri (new cold suites) | `$env:MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks"; cargo +nightly miri test --test trap_proof --test cold_diagnostics --test nuclear_vs_shedding --test flight_diagnostics` | 22 tests ok (0 UB; trap 8/8 in ~50s) |
| torture release | `cargo test --release --test cycle_torture` | ok |
| clean rebuild | `rmdir /s /q target` then the gates above | — |

After ANY clean: `CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo test`.
That is the environment, not the code. Never "fix" it by editing Rust.

## 9. Hot contract (what lives where — no improvisation)

| Under lock (A/C) | OUTSIDE lock (B / `emit`) |
|---|---|
| `Copy` moves, `saturating_add/sub`, flag flips | `format!`, `String`, `Vec::clone`/`push` |
| one `Arc` bump (`hooks`) | `stats.clone()`, `record.clone()`, `fallback.clone()` |
| policy lookup, `console_used` integers | `friction_fn`, hooks, `custom`, `fallback()`, console text |
| `BlockCaller` verdict integers | `BlockCaller` sleep (`std::thread::sleep`, lock released) |

`record_locked(kind: FaultKind, bump: u8, extra: u64)` — codes, never a
closure. One owner per knob: the parking sleep lives ONLY in
`enter_inner`; the console text ONLY in `emit`; the hook chain ONLY in
`watchdog::isolate_once`. No dead settings: every builder flag has a
consumer + a test (nanoclick "no dead settings" rule, snubber edition).

Owner table (R1 — one owner, no improvisation):

| Knob | Owner ONLY | Observers (read-only) |
|---|---|---|
| parking sleep (`BlockCaller`) | `enter_inner` | nobody (caller thread parks itself) |
| console text | `emit` | nobody (stderr sink) |
| hook chain | `watchdog::isolate_once` | hooks see `(&record, &stats, &policy)`, mutate nothing |
| breaker verdict | `record_locked` / `note_outcome` / `reset_breaker` | `enter_inner` reads the flag, never writes it directly |
| fallback value | builder arms, `fallback_value()` calls outside lock | UI polls, never arms |
| rollback body | guard owner (`take` + `drop`) | node counts the settle, never runs the body |

Veto: a new builder flag without exactly one consumer + one test is a
review-blocking defect (dead setting).

## 10. F-catalog: documented corpses (symptom → autopsy → veto)

- **F1 fake-green (`1 = 1+0+0`).** 40k attempts, `started=1`, test green.
  Veto: conservation + hard floor + per-kind counters (§4). Ref: `cycle_torture`.
- **F2 breaker OFF trap.** `trip_after_failures(0)` kills streak, not
  `Panic→TripBreaker` policy → `breaker=39999`. Veto: OFF-honoring trip +
  explicit Panic override in torture; trips covered by reflex tests.
- **F3 epoch race.** Global allocator vs single-lane gate → `stale=34992`.
  Veto: allocator + counted bounded retry (cap 64); hidden retry = cardboard.
- **F4 dead `Duration::from_millis(1)`.** Built, never slept. Veto: no
  statement without effect; `clippy` + review.
- **F5 hollow `|| {}` rollback.** Drop ran void, books moved. Veto: REAL
  `fn rollback` + `rollbacks == aborts + drops`.
- **F6 half-open stampede myth.** 8 probes ≠ 1+7: first `commit()` closes
  synchronously. Veto: two-wave test (1 probe → 8 flood), documented.
- **F7 wall mis-accounting.** 2nd-level `StaleEpoch` counted as `BULK`
  (delta 42). Veto: separate counters per `Err` variant.
- **F8 `+=` under lock.** Release overflow = panic under mutex. Veto:
  `saturating_*` + `hard_clamp_slots`, never `debug_assert`.
- **F9 console_slot duplication.** Two identical arms `(Some,Some)/(Some,None)`.
  Veto: single arm (fixed 2026-10-09).
- **F10 cold paths under lock.** `stats.clone()` / `fallback.clone()` /
  `metric_samples` Vec allocate under a short lock. Veto: mark HONEST NOTE,
  keep admission ZERO-PANIC, count poison. SCOPE: `stats()` once per
  second is fine — but at 160 CPS in a hot loop the `last_fault` clone
  WILL show up in the profiler. Anyone calling `stats()` hot must
  snapshot rarely and cache, or split counters from `last_fault`.
  Trigger for rework: profiler names `stats`/`metric_samples`.
- **F11 foreign code under lock.** `friction_fn`/hook/custom under mutex =
  deadlock + poison. Veto: PHASE B only, re-lock + re-check (no TOCTOU).
- **F12 closure smuggling.** `bump: impl FnOnce` could hide a panicking
  call under lock. Veto: integer codes in signature.
- **F13 borrowed lock illusion.** Trusting the node's mutex to order
  FOREIGN state (rollback writes observed via `Relaxed`, guard passed
  through an unsynchronized handoff). The lock guards node books only;
  rollbacks run outside it; Floor-1 owns no lock. Veto: 4th MUST in
  `src/contract.rs` + `Release`/`Acquire` (or own lock) on foreign
  state; `Relaxed`×2 is a review-blocking defect.
- **F14 thread sleep in async.** `BlockCaller` / `AdaptiveBlock` park the
  OS thread via `std::thread::sleep` — under Tokio this freezes the
  whole worker (4 sleeps = dead server for 50 ms). Veto: async paths use
  `AdviseOnly` ONLY; sleep via `tokio::time::sleep(retry_after_ms)`.
  Never invent channel APIs: `std::sync::mpsc::Sender` has NO `try_send`
  (only `SyncSender` does) — check std signatures before writing.
  No runtime semaphore in the crate by design (zero-dep, no runtime):
  `tokio::sync::Semaphore::acquire` lives in user code. BUT the crate
  OWNS the async wait itself since v0.5: `enter_async()` parks the
  `core::task::Waker` at 0% CPU (no runtime needed — `Future` is `core`),
  `enter_paced_async` sleeps via YOUR injected timer, `set_stage`
  survives `select!` kill, `try_send` rides bounded `SyncSender`.
  The crate refuses fast OR parks honestly — never spins, never blocks.

## 11. Diagnostics: where to look

```powershell
cargo test --test cycle_torture -- --nocapture | findstr REALITY
cargo test --test reflex_bulkhead -- --nocapture | findstr WALL
# Heavy 10,000 OS TerminateProcess Chaos Monkey (~3 min, spawns 2 GUI windows):
cargo test --test chaos_dual_terminal -- --ignored --nocapture
```

`[REALITY] attempts/started/settled/bulk/stale/breaker/poison`,
`[WALL] attempts/started/settled/bulk(node)/stale(node)`.
Panic noise `telemetry plugin died` on stderr is EXPECTED (rotten-hook
isolation working, caught by `isolate_once`).

Proof, not prose (R4): a bug report without a `[REALITY]` / `[WALL]`
print is a theory, not a fix. Paste the line, then the verdict.

## 12. Release canon + versions

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo check --no-default-features
cargo test --release --test cycle_torture
```

Green = all above green in ONE run, same commit updates `README.md` +
`AGENTS.md` + `tech.md` numbers. Version lives ONLY in `Cargo.toml`
(`name`/`version`); docs quote it, never redefine it. Current:
**v0.1.0**, **142 + 12 = 154 green**, MSRV 1.70.

Publication honesty (R7): `.gitignore` has been added covering `target/`,
local scratch logs, python artifacts, and OS temp files. Cargo package exclusions
are explicitly tracked in `Cargo.toml [package] exclude`.

## 13. Silicon boundary: where snubber ends (R5+R6)

Radius: snubber guards a SYNCHRONOUS perimeter with unwinding
(`?` / panic-unwind). Outside it — honest contracts, not code:

- **Cooperative watchdog only (R5).** `check_timeout()` fires where the
  worker CAN reach (loop iteration / progress emit). The node never
  preempts a foreign thread; no killer threads, ever.
- **Hung Win32 / kernel zombie.** A thread frozen in the driver holds no
  Rust lock the crate can release. Rescue = external supervisor:
  `stats()` + `last_fault` → heartbeat lost → `TerminateProcess` on the
  satellite. NEVER `TerminateThread` in-process (heap/loader lock stays
  wedged — see `docs/FUTURE_TRAPS.md` Trap 2). Satellite pattern
  (Chrome/DAW): the worker is a disposable; the parent releases iron
  itself and reaps the corpse from outside.
- **Dead wall: `panic = "abort"` / SIGKILL / D-state (R6).** No tables,
  no `Drop`, no guard moves a micron — the process is gone in one tick.
  Inside the fallen process you are powerless; survival is ONLY an
  external daemon + shared-memory heartbeat that releases iron from
  outside. Do not sell the knife as a spaceship.

## 14. 🚫 Anti-Pattern & Tone Contract: Zero-Theatrics Policy

### 1. Заборона на підлітковий "Grimdark" та кітч (No Edge-Lord Naming)
- **СУВОРО ЗАБОРОНЕНО** використовувати анатомічні, криваві або хоррор-метафори в назвах файлів, функцій, прикладів, коммітів та документації (`aorta`, `blood`, `autopsy`, `corpse`, `flesh`, `meat`, `deathmatch`).
- **СУВОРО ЗАБОРОНЕНО** іменувати стрес-тести або верифікаційні стенди як гладіаторські бої (`Gauntlet`, `Thunderdome`, `Arena`, `Gladiator`).
- **Стандарт:** Мова документації та прикладів має відповідати стандартам авіоніки, баз даних та ядра Linux/SQLite:
  - Замість `torture_ruptured_aorta` → `heavy_pipeline_stress`
  - Замість `field_rusty_autopsy` → `flight_recorder_diagnostics`
  - Замість `Gauntlet` → `Formal Invariant Verification` / `INVARIANTS.md`

### 2. Заборона на фальшиві фічі та привласнення std (No Bogus Patterns)
- **СУВОРО ЗАБОРОНЕНО** видавати можливості стандартної бібліотеки Rust (`std::sync::Mutex`, `PoisonError::into_inner()`, `std::panic::catch_unwind`) за унікальні винаходи чи фічі `snubber` (як-от псевдо-патерни "Snubber SafeLock").
- Кожен приклад в `examples/` зобов'язаний вирішувати реальну практичну проблему суто через публічні примітиви `snubber` (`SnubberGuard`, `SnubberNode`, `scoped`, `TaskConduit`, `Supervisor`).

### 3. Тон комунікації: Інженерна Тверезість (Quiet Power)
- `snubber` — це критичний інфраструктурний запобіжник. Сила бібліотеки доводиться 188+ зеленими тестами, детермінованим фаззером на 1 МБ шуму та апаратними бар'єрами, а не знаками оклику чи зухвалими заявами «Зламай нас, якщо зможеш».
- Тон документації — стриманий, точний, важкий, практичний.

### 4. Термінологічний канон (Terminology Canon)
- **Заборонено змішувати «Floors» і «Levels»:** використовується виключно єдина драбина архітектурних шарів:
  - **Layer 1: Core RAII Guard** (`core`-only, stack guard, `EpochGate`, ~1.5 ns)
  - **Layer 2: Concurrency Bulkhead** (`std`, `SnubberNode`, circuit breaker, priority lanes)
  - **Layer 3: Async Waker Queue** (0% CPU, runtime-agnostic)
  - **Layer 4: Process Supervisor** (IPC pipe, EOF drain, token rescue)
  - **Layer 5: Diagnostics & Health** (Telemetry, `SnapshotV1`, CI gates)
- **Заборонено косплей підводних човнів і кінотрейлерів:**
  - Замість «Nuclear vs Hydraulic» / «Hull-breach» → **`Fatal Errors vs Operational Backpressure`** (або `Catastrophic Faults vs Capacity Shedding`)
  - Замість «Pure Iron Latency» → **`Zero-Allocation Latency (~1.5 ns)`**
  - Замість «Newton's 3rd Law» → **`Adaptive Backpressure & Pacing`**
  - Замість «Cockpit Protocol» → **`Bi-directional Task Steering`** (або `Task Conduit`)
  - Замість «boundary shim» у дескрипшені → **`failure-recovery engine`**
  - Замість `chaos_sniper` → **`stress_harness`**


