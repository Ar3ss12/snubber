# PLAN.md — виправлення за підсумками ворожого аудиту

> **Стан на 2026-10-25:** HEAD `784453c release: 0.6.9 — audit closeout`,
> `Cargo.toml` = `0.6.9`, теги `v0.5.0`, `v0.5.3`, `v0.6.3` (`v0.6.9` не створено — потрібен окремий наказ).
> Етапи 0–8 закриті; suite 404 (378 + 26 doctests), бейдж 404.
> **Джерело:** повний звіт аудиту (секції 1–3) + доказова база під-агентів, зібрана в цій сесії.
> **Правило етапу:** етап не закривається без `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
> повного suite, скриптових гейтів і оновленого CHANGELOG (якщо поведінка змінилася).
> **Обмеження проєкту:** zero dependencies, MSRV 1.70, `no_std` core, `#![forbid(unsafe_code)]`,
> small-RAM box (`cargo -j 2`, incremental off), `.ps1` — тільки ASCII, локальні docs
> (`AGENTS.md`, `tech.md`, `docs/DECISION_MATRIX.md`, `FUTURE_node_split.md`, `CODEBASE_REPORT.md`,
> `scripts/analyze_codebase_full.py`, `RESEARCH_ECOSYSTEM_BINDINGS.md`) не комітяться і не читаються тестами.
> **Теги:** тільки за окремим наказом. Без наказу — version bump без тегу.

---

## 0. Легенда

| Позначка | Значення |
|---|---|
| `[ ]` | не зроблено |
| `[~]` | у роботі |
| `[x]` | зроблено й перевірено |
| `[!]` | заблоковано (вказати причину) |
| P0 | ламає семантику/облік або дає фальшиво-зелений тест |
| P1 | небезпека під навантаженням, діра в CI/скриптах |
| P2 | документація, покриття, косметика |
| P3 | теоретичне/відкладене, задокументувати |

Кожен етап має: **Мета → Файли → Кроки (чекбокси) → Тести → Готово → Коміт → Дописи**.
«Готово» — це об'єктивний критерій, а не «код написано».

---

## 1. Зведена таблиця знахідок

| ID | P | Знахідка | Доказ | Етап | Стан |
|---|---|---|---|---|---|
| F-01 | P0 | Паніка в `friction_fn`/`pressure_fn` лишає `half_open_probe=true` назавжди; breaker заварено | `src/node.rs:3008`, `3092-3100`, `3154-3159`, `2996-3006`; єдиний `catch_unwind` — `1469` | 1 | `[x]` |
| F-02 | P0 | `SnubScanner::push` віддає кадр двічі після перехопленої паніки `on_token` (`take=0` повторно входить у `body_len==40`) | `src/drain.rs:215-218` | 1 | `[x]` |
| F-03 | P0 | `publish_fault` доккоментар «no allocation, no panic» хибний: `NodeName::Owned` дропається під замком; `flight` може реалокати | `src/node.rs:1937-1954`, `src/name.rs:26-30`, `4264-4271` | 1 | `[x]` |
| F-04 | P0 | `pool.report()` алокує `Vec`, доки двічі обіцяно «no allocation» | `src/pool/mod.rs:252-254`, `src/pool/report.rs:116-121`, `README.md:132`, `USER_GUIDE.md:1789` | 1/6 | `[x]` |
| F-05 | P0 | Тести консервації перевіряють урізані рівняння; повний 7-доданковий закон не проганяється | `src/node.rs:289-297`; `tests/bulkhead_capacity.rs:97-110`; `tests/transition_orphan.rs:83-85`; `tests/stress_breaker_churn.rs:162-165` | 3 | `[x]` |
| F-06 | P0 | Бенчі конкуренції можуть опублікувати ns/op за нуля допусків: retry виснажується, `stale` не читається, `join` ковтається | `benches/admission_bench.rs:625-665`, `679-711`, `730-757` | 4 | `[x]` |
| F-07 | P1 | Drop `Waker` під node-локом (`slot.waker = waker`, `retain`) | `src/node.rs:3678`, `3896` | 2 | `[x]` |
| F-08 | P1 | Алокація `Vec<Waker>` під замком wakers у conduit | `src/conduit.rs:399-402` | 2 | `[x]` |
| F-09 | P1 | `SystemTime::now()` під node-локом у rate limiter і `check_timeout` | `src/node.rs:3299`, `5277` | 2 | `[x]` |
| F-10 | P1 | CI міряє лише 1 бенч; решта 51 і перформанс-регресії не гейтяться; артефактів немає | `.github/workflows/ci.yml:259-267` | 5 | `[x]` |
| F-11 | P1 | Semver-гейт сліпий до проміжних релізів (baseline = останній тег `v0.6.3`, crate `0.6.8`) | `scripts/check_api_diff.sh:55-65`; `git tag` | 5 | `[x]` |
| F-12 | P1 | PS-гейт вужчий за sh: 6 лічильників vs 15, лише `record_locked` vs 3 функції | `scripts/check_compile_gates.ps1:25-94` vs `.sh:52-120` | 5 | `[x]` |
| F-13 | P1 | `stress_harness.sh`: SKIP не валить рана; PID-гейт у секундах; JSON — regex; `LAUNCH=0` вимикає гейти | `scripts/stress_harness.sh:108-146`, `370-373`, `396-398`, `420-443` | 5 | `[x]` |
| F-14 | P1 | Miri: `-Zmiri-ignore-leaks` у всіх лейнах; покрито 33 з ~60 файлів | `.github/workflows/ci.yml:175,180,216` | 5 | `[x]` |
| F-15 | P1 | `feed`/`finish`/`write_to` — std-only, а таблиця гайда продає їх як core/bare-metal | `src/supervisor.rs:518,678,888,907`; `src/lib.rs:128-129`; `USER_GUIDE.md:1787` | 6 | `[x]` |
| F-16 | P1 | `PipeDrain::feed` після `finish` тихо відкидає чанк і рапортує `0 frames`; коментар про clock суперечить коду | `src/supervisor.rs:892-918`, `896-901` | 2/7 | `[x]` |
| F-17 | P1 | FakeWake: мертве поле `usize`, `wake` — no-op, identity-твердження не перевіряється | `tests/task_engine.rs:255-268` | 5 | `[x]` |
| F-18 | P2 | `USER_GUIDE.md` документує неіснуючі `pool.attach`/`pool.note_death` | `USER_GUIDE.md:1788` vs `src/pool/lane.rs:157,241` | 6 | `[x]` |
| F-19 | P2 | Назви бенчів у README не збігаються з джерелом (`_threads_`, `classify_stall`) | `README.md:102,154-156` vs `benches/admission_bench.rs:49,627,679,719` | 6 | `[x]` |
| F-20 | P2 | Числа латентності без артефактів; бейдж 394 без дати/команди; «121 ns» не існує | `README.md:7,67,94,108`; `target/` відсутній | 5/6 | `[x]` |
| F-21 | P2 | Публічні аксесори не задокументовані (pool/envelope/harness/token) | `src/pool/mod.rs:121,131,277`; `lane.rs:65,145`; `envelope.rs:84-196`; `harness.rs:28-198`; `drain.rs:109` | 6 | `[x]` |
| F-22 | P2 | `supervisor_pool_stress`/`stress_mixed`/`stress_lease`: `env_num` без мінімумів → `KILLS=0` робить усе тривіально зеленим | `tests/supervisor_pool_stress.rs:37-38`; `stress_mixed.rs:45-46`; `stress_lease.rs:64-67` | 3 | `[x]` |
| F-23 | P2 | `transition_orphan` «preempt storm» синтетичний (`report(Preempted)`), не справжній preemption | `tests/transition_orphan.rs:69-73` | 3 | `[x]` |
| F-24 | P2 | `priority_preemption.rs:119` rollback-свідок не мутує стан, лише лічить виклики | `tests/priority_preemption.rs:50,113-119` | 3 | `[x]` |
| F-25 | P2 | `tests/erased.rs` rollback-свідки без node-stat witness | `tests/erased.rs:39,65` | 3 | `[x]` |
| F-26 | P2 | Два різні `static ROLLBACKS` з однаковим ім'ям | `tests/guard.rs:234,255` | 7 | `[x]` |
| F-27 | P2 | Приклад ISR: cycle 3 abort без post-abort assert | `examples/12_isr_bridge.rs` | 7 | `[x]` |
| F-28 | P2 | Покриття async-дір: settle-pop→drop, HalfOpen+cancel, poison async | `tests/async_primitives.rs`, `tests/conduit.rs` | 7 | `[x]` |
| F-29 | P2 | `stress_harness.ps1`: managed-path без image/creation gate; kill-semantics/verdict розбіжні з py | `scripts/stress_harness.ps1:294,343,360-367` | 5 | `[x]` |
| F-30 | P2 | `check_api_diff` при недоступному git: `hash-object` → `""` двічі, lock-мутація невидима | `scripts/check_api_diff.sh:63-72` | 5 | `[x]` |
| F-31 | P2 | `check_compile_gates.sh` pipe/grep failure → `OK`; `CI=false` трактується як CI | `scripts/check_compile_gates.sh:14,110-119` | 5 | `[x]` |
| F-32 | P2 | `bench.sh:147` при помилці `grep` публікує; `:163` тихо пропускає summary | `scripts/bench.sh:147,163` | 5 | `[x]` |
| F-33 | P2 | `check_compile_gates.ps1` друкує `OK` після FAIL baseline==HEAD | `scripts/check_compile_gates.ps1:194-197,218` | 5 | `[x]` |
| F-34 | P2 | Планові/ручні CI-лейни (PS TerminateProcess, raw-kill, euthanasia) не мають PR/push-еквівалента | `.github/workflows/stress.yml:134,171,211` | 5 | `[x]` |
| F-35 | P3 | TTL re-stamp після settle-wake може нескінченно продовжувати TTL; TTL перевіряється лише на poll | `src/node.rs:3730`, `3679-3684`; `tests/task_engine.rs:272` | 7 | `[x]` |
| F-36 | P3 | `auto_seq` Relaxed: епоха, видана «пізніше», може програти гонку `gate.observe` | `src/node.rs:2308` | 7 | `[x]` |
| F-37 | P3 | `LaneState` cross-field recovery: теоретично `pid:Some` + `drain:None` | `src/pool/lane.rs:128-168` | 7 | `[x]` |
| F-38 | P3 | `channel.rs:300` Acquire на лічильнику без Release-письменника (декоративний) | `src/channel.rs:300` | 7 | `[x]` |
| F-39 | P3 | `node.rs:1462/1482` SeqCst fences ні з чим не паряться (no-op) | `src/node.rs:1462,1482` | 7 | `[x]` |

---

## 2. Етапи

### Етап 0 — Baseline і захист від регресій (P0)

**Мета:** зафіксувати відтворюваний baseline, щоб кожен наступний коміт мав доказове «до/після»,
і щоб бейдж/цифри не розходилися з деревом.

**Файли:** `scripts/` (новий `check_test_count.sh`), `README.md`, `PLAN.md`.

**Кроки:**
- [x] `cargo fmt --check` на чистому дереві; зафіксовано OK.
- [x] `cargo clippy --all-targets --all-features -- -D warnings`; зафіксовано OK (18.56 s на baseline).
- [x] `cargo test --all-targets --all-features -j 2`; зібрано `^test result:` у суму; doctests враховані.
- [x] `bash scripts/check_compile_gates.sh`; зафіксовано 6 OK + Gate 6 SKIP (semver-checks не встановлено локально).
- [x] Додати `scripts/check_test_count.sh`: рахує фактичні passed + doctests і порівнює з числом у бейджі
      `README.md:7`; exit 1 при розбіжності, exit 2 якщо suite упав, floor 300 від хибного парсингу.
      Підключено в CI (`.github/workflows/ci.yml`, job `test-matrix`, Linux): парсить наявний
      `target/test-suite.log`, тому другий прогін suite не потрібен.
- [x] Додати в README поряд з бейджем provenance-коментар: команда + дата останньої перевірки.
- [x] Перевірити вільне місце на диску; `target/` зникав раніше — зафіксовано як environmental limit.

**Тести:** `check_test_count.sh` — позитивний прогін на реальному лозі (397=397, OK) і негативний на
копії README з `999` (exit 1).

**Готово:** baseline-числа записані в PLAN.md (нижче), скрипт падає на штучно зміненому бейджі.

**Коміт:** `chore(ci): add reproducible test-count gate and record audit baseline`

**Дописи:**
- Baseline після Етапу 1: suite **397 passed / 0 failed / 5 ignored**, doctests входять у суму = **397**.
  Бейдж `397` збігається; gate підтверджує це на кожному Linux-прогоні CI.
- «394» був baseline до трьох нових регресій Етапу 1; число в README оновлено разом із ними.
- `target/criterion` у дереві відсутній, тому жодне опубліковане число латентності зараз не відтворюється;
  це фіксується як стан, а не «переміряно».
- Скрипт рахує саме passed, а не «скільки рядків»: інакше `ignored`/`0 passed` дадуть хибний збіг.
- Проведено в job `test-matrix` замість `quality`, щоб не ганяти suite двічі; лог уже є там.

---

### Етап 1 — Критична коректність (P0)

**Мета:** прибрати стани, у яких код бреше сам собі: заварена проба, дубль кадру, хибний доккоментар,
хибна обіцянка «no allocation».

**Файли:** `src/node.rs`, `src/drain.rs`, `src/pool/mod.rs`, `README.md`, `docs/USER_GUIDE.md`,
`tests/` (нові регресії).

#### 1.1 F-01 — panic-safe probe

- [x] Знайти **всі** виклики `friction`/`pressure`/`fallback`/hook-замикань у sync та async шляхах
      (grep `snap_friction`, `snap_pressure`, `fallback`, `react(`, `hooks`).
- [x] Додати один helper (`fn run_admission_hook<R>(&self, f: impl FnOnce() -> R) -> R`):
      `catch_unwind(AssertUnwindSafe(f))`; на `Err(payload)` — під свіжим замком `half_open_probe = false`,
      потім `resume_unwind(payload)`. Ніякого settle/книг: транзакція ще не допущена.
- [x] Замінити **кожен** виклик hook'а на helper. У sync `enter_inner` це два місця: friction
      (`src/node.rs`, гілка `snap_friction`) і pressure (`react(bp)` для `pressure_fn` та
      `BackpressurePolicy::Custom`). `friction_wait_ms`/`fallback_value` кличуть handler'и поза замком,
      але не озброюють `half_open_probe`, тому вони не входять у цей контракт.
- [x] Перевірити, що helper не ковтає паніку і не змінює `Result`-контракт при `Ok`.
- [x] Регресії:
      `panicking_friction_hook_does_not_wedge_half_open_probe` (sync),
      `panicking_pressure_hook_does_not_wedge_half_open_probe` (sync).
      Async-версія: **N/A** — async-шлях не викликає `friction_fn`/`pressure_fn`, тому заварювання
      проби там відтворити неможливо; контракт закритий sync-тестами.

**Дописи:** `catch_unwind` доступний лише в `node` (std-модуль), тому helper живе там; core не чіпаємо.
`AssertUnwindSafe` виправданий: після паніки ми не довіряємо жодному захопленому стану, лише чистимо прапорець.
Кожен тест тримає єдиний слот bulkhead, тому проба HalfOpen гарантовано потрапляє в гілку hook'а.

#### 1.2 F-02 — scanner: reset до доставки

- [x] У `SnubScanner::push` замінити порядок на:
      `let token = ProcessToken::from_body_bytes(&self.body); self.reset(); frames += 1; on_token(token);`
- [x] Перевірити, що `reset()` очищає `window`, `body`, `body_len`, `in_body` (і що це не ламає
      частковий кадр у наступному чанку).
- [x] Регресія `panicking_on_token_does_not_redeliver_frame`: `catch_unwind` навколо `push` з панікуючим
      `on_token`, потім `push` того самого байтового потоку → рівно один виклик `on_token`, не два.

**Дописи:** `frames` інкрементується до `on_token`, тому при паніці значення все одно втрачається
(функція розмотується). Головне — стан сканера вже скинутий; повторної доставки немає.

#### 1.3 F-03 — publish_fault: drop поза замком + чесний доккоментар

- [x] Замінити `inner.stats.last_fault = Some(owned);` на
      `let old = inner.stats.last_fault.replace(owned);` і `drop(old);` після виходу з-під guard'а.
- [x] Перевірити, що `push_flight_inner` не може реалокати на стабільному шляху: `flight` створюється
      з `VecDeque::with_capacity(flight_depth.max(1))`; poison-recovery не зменшує його.
- [x] Виправити доккоментар (`src/node.rs`, `publish_fault`): «no foreign code on the steady path; the
      replaced `FaultRecord` is freed after unlock; a one-time flight realloc is possible only after an
      external shrink».
- [x] Регресія: наявні fault-серії з `NodeName::Owned` покривають коректність `last_fault.node()`;
      окремий deadlock-тест не додано — drop під замком не спостерігається, бо старого запису під
      guard'ом більше не існує (структурний доказ + доккоментар).
- [ ] (Опційно) інструментований тест, що drop `Arc<str>` не відбувається під замком — свідомо
      відхилено: вимагав би тестового hook'а в production-коді.

**Дописи:** не можна просто «не дропати» старий запис — пам'ять потече. Єдиний коректний варіант —
`replace` + drop після відпускання guard'а. `flight_depth == 0` більше не дає першу реалокацію на fault.

#### 1.4 F-04 — docs: прибрати «no allocation» у `report()`

- [x] `README.md:132`: замінено на «one `Vec<LaneSnapshot>` allocation (`lanes.len()` rows) included in the number».
- [x] `docs/USER_GUIDE.md:1789`: те саме; число ~111 ns явно включає алокацію.
- [x] Перевірено: `expired_lanes` подібної обіцянки не має.
- [x] Grep `no allocation` по README/USER_GUIDE: решта збігів істинні (conflate, `ProcessToken`, `NodeName::Static`, `fmt::Write`).

**Тести:** doc-гейт не потрібен; достатньо `cargo test --doc` і перевірки grep-ом у Stage 6.

**Готово:** паніка в hook'ах більше не заварює breaker; сканер не дублює кадр; `publish_fault`
не дропає foreign пам'ять під замком; докси `report()` чесні; усі нові тести зелені, suite не зменшився.

**Коміт:** `fix(core): panic-safe probe hooks, scanner reset order, publish_fault lock hygiene`

**Дописи:**
- F-01 і F-02 — це семантичні баги, а не «стиль»: перший вічно блокує вузол, другий дублює бізнес-подію.
- F-03 не є memory-unsafe, але порушує власний контракт критичної секції; виправляється без зміни API.
- F-04 — тільки docs; код `report()` лишається алокувати, бо це холодний шлях.

---

### Етап 2 — Гігієна замків (P1)

**Мета:** прибрати foreign-код/алокації з-під node/conduit/wakers замків там, де це можливо без
зміни семантики; решту — задокументувати як свідомий компроміс.

**Файли:** `src/node.rs`, `src/conduit.rs`, `src/supervisor.rs`, `tests/async_primitives.rs`.

#### 2.1 F-07 — `Waker` drop під node-локом

- [x] `node.rs` re-park: `let old = core::mem::replace(&mut slot.waker, waker);` → `drop(inner); drop(old);`
      → `Poll::Pending`. TTL-origin (`enqueued_at_ms`) і `evicted_out` не залежать від guard'а.
- [x] `deregister_waiter`: замість `retain` (дропає під замком) — `position` + `VecDeque::remove`
      (флаг ідентифікує рівно один запис), видалений `ParkedWaiter` дропається після unlock.
      Handoff-`pop_front` лишається під замком, `wake()` — поза ним. Алокацій немає взагалі.
- [x] `Arc::new(AtomicBool)` для `evicted` лишено під замком і задокументовано: підняти алокацію над
      `self.lock()` означало б алокувати на кожному допущеному poll'і заради холодного шляху парку.
- [x] `waiters.push_back`: `VecDeque::new()` лишається; ріст — cold-path, коментар додано разом з
      F-07 (див. вище). Окремий `with_capacity(64)` відхилено: 64 саркастично більше за типовий
      `queue_limit`, а пам'ять платилася б завжди.
- [x] Регресії: `re_park_replaces_waker_once_and_wakes_only_the_new_one` (node) і
      `re_poll_replaces_parked_waker_and_wakes_only_the_new_one` (conduit). Структурний тест «не під
      замком» неможливий без інструментації production-коду — поведінковий тест ловить втрату waker'а.

**Дописи:** identity-флаг унікальний на запис, тому `position`+`remove` еквівалентний `retain`, але
повертає видалене значення назовні. `VecDeque::remove` — O(n) як і `retain`, черга обмежена
`queue_limit`.

#### 2.2 F-08 — conduit: `mem::take` замість `collect` під замком

- [x] `wake_async`: `core::mem::take(&mut *wakers)` передає наявний `Vec` без алокації під замком;
      `wake()` викликається після відпускання guard'а.
- [x] `ResumeFuture::poll`: `slot.waker = waker` → `mem::replace` + `drop(wakers)` + `drop(old)`.
- [x] Регресії: `re_poll_replaces_parked_waker_and_wakes_only_the_new_one`; наявні
      `resume_never_loses_the_wakeup_under_pause_churn` і `dropped_resume_future_removes_its_parked_waker`
      лишаються зеленими.

#### 2.3 F-09 — clock поза замком

- [x] `enter_armed_with_priority`: `let entered_at_ms = now_ms();` до `self.lock()`; token bucket
      використовує знімок. Час, витрачений на взяття замка, кредитується **наступним** допуском —
      токени ніколи не перевипускаються.
- [x] `check_timeout`: `settled_at_ms` знімається після rollback (тривалість rollback увіходить
      у `elapsed_ms`) і до `node.lock()`; запис у `FaultRecord` більше не кличе `now_ms()`.
- [x] Регресії: наявні rate-limit тести (`rate_per_sec > 0`) проходять; детермінованого injectable
      clock у node немає, тому окремий clock-тест не додано (smoke-покриття збережене).

**Дописи:** `now_ms()` читає OS clock — системний виклик, який не має виконуватися під м'ютексом
вузла. Консервативний знімок не змінює контракт лімітера, лише зсуває момент обліку.

#### 2.4 F-16 — `feed` після `finish`

- [x] `PipeDrain::feeds_after_eof()`: окремий `Arc<AtomicU64>` (не поле `DrainCounters` — додавання
      поля в конструктивний публічний struct зламало б semver). Інкремент на обох EOF-перевірках.
- [x] `feed`: lock-free fast path `eof()` → лічильник → `0`; повільний шлях читає clock **до**
      `ingest.lock()` і повторно перевіряє EOF під замком (авторитетна перевірка).
- [x] Коментар виправлено: clock більше не викликається під жодним drain-замком; `ClockFn` — caller
      code.
- [x] Тест `finish_drops_the_torn_tail_and_seals_the_drain` розширено: два post-EOF чанки → рівно
      `feeds_after_eof() == 2`, `finish()` не рахується.

**Дописи:** `DrainCounters` — публічний struct без `#[non_exhaustive]`, тому нове поле змусило б
мажорний бамп; окремий аксесор зберігає сумісність 0.6.x. Лічильник означає «байти відкинуто, не
забуферено» — це telemetry для продюсера, що пише в закритий pipe.

**Готово:** жоден `Waker` не дропається під node/wakers замком; conduit не алокує під замком;
clock-читання не в критичній секції; `feed`-після-EOF спостережуваний; suite зелений.

**Коміт:** `fix(concurrency): keep waker drops and clock reads outside mutexes`

**Дописи:**
- Це не memory-safety, а latency/panic-safety: drop `Waker` може викликати код екзекутора, який
  панікує або бере власні замки. Ризик deadlock залежить від екзекутора, тому фікс — обов'язковий.
- Якщо двофазний `retain` виявиться надто складним/race-prone — залишити `retain` під замком,
  але тоді **явно** задокументувати виняток у доккоментарі й додати це в «Закрито без змін» з причиною.
  Не робити вигляд, що контракт виконано.

---

### Етап 3 — Консервація та чесні тести (P0/P1)

**Мета:** жоден тест не має проходити на нулі операцій або на урізаному рівнянні.

**Файли:** `tests/invariants.rs`, `tests/bulkhead_capacity.rs`, `tests/stress_breaker_churn.rs`,
`tests/transition_orphan.rs`, `tests/priority_preemption.rs`, `tests/erased.rs`,
`tests/supervisor_pool_stress.rs`, `tests/stress_mixed.rs`, `tests/stress_lease.rs`.

**Кроки:**
- [x] `tests/invariants.rs`: додати повний 7-доданковий закон
      `attempts == total_started + bulkhead + stale + breaker + rate + poisoned + memory`;
      зберегти існуючий bucket-enum; підняти floor для `accepted` (не `> 0`, а, наприклад, `>= attempts/10`
      для стрес-сценарію — підібрати так, щоб не флапало).
- [x] `tests/bulkhead_capacity.rs:97-110`: додати `assert!(started > 0)`, повний закон, і перевірку
      кожного лічильника окремо (не тільки `bulk + stale`).
- [x] `tests/bulkhead_capacity.rs:139-176`: замінити `else { refused += 1 }` на розбиття за
      `EnterError::{BulkheadFull, StaleEpoch, RateLimited, BreakerOpen, ...}` і звірити кожен бакет.
- [x] `tests/stress_breaker_churn.rs`: додати
      `admitted + bulk + breaker + stale == THREADS*OPS_PER_THREAD`; підняти floor
      (`failed > 0 && committed > 0` лишити, але додати `admitted >= N`).
- [x] `tests/transition_orphan.rs`: перейменувати коментар «preempt storm» → «synthetic anomaly feed»;
      додати `started > 0` floor; якщо можливо — додати справжній preemption-сценарій окремим тестом
      (Critical over Background), інакше явно позначити синтетику.
- [x] `tests/priority_preemption.rs`: rollback має мутувати незалежний стан (`AtomicBool`/`AtomicU32`
      «resource_restored»), а не лише лічити виклики; assert на стан після abort.
- [x] `tests/erased.rs`: додати node-stat witness (`implicit_drops`/`explicit_aborts`), а не тільки
      локальний `RELEASES`/`fired`.
- [x] `tests/supervisor_pool_stress.rs:37-38`, `stress_mixed.rs:45-46`, `stress_lease.rs:64-67`:
      `env_num(...).max(MIN)` з `MIN >= 2` для lanes/kills; якщо env заданий нижче — `panic!` з поясненням.
- [x] `tests/guard.rs:234,255`: перейменувати статики (`ROLLBACKS_COMMIT`, `ROLLBACKS_DROP`), щоб grep
      не подвоював свідчення.

**Тести:** самі змінені тести + повний suite. Додатково: «негативний» прогін — тимчасово зламати
лічильник (локально, не комітити) і переконатися, що кожен новий assert падає.

**Готово:** кожен conservation-тест має позитивний floor і повний набір бакетів; `KILLS=0`/`LANES=0`
неможливі; suite зелений, кількість тестів не зменшилась.

**Коміт:** `test(conservation): pin full admission law and positive volume floors`

**Дописи:**
- Floor має бути детермінованим: для стрес-тестів краще `>= some_fraction`, ніж точна рівність,
  інакше CI флапатиме на повільних ранерах.
- `bulk + stale > started` — не закон, а евристика; після повного закону її можна прибрати або
  залишити як sanity-check.
- Негативний прогін виконано (локально, без коміту): видалення інкременту `bulkhead_rejections`
  у `record_locked` валить `bulkhead_capacity` (2 тести), видалення інкременту
  `stale_epochs_rejected` валить `invariants::verify_slot_containment_and_single_shot_rollback`.
  Обидва саботажі відкочено через `git checkout -- src/node.rs`.

---

### Етап 4 — Чесність бенчів (P1)

**Мета:** бенч не публікує число, якщо вимірював не те, що заявлено.

**Файли:** `benches/admission_bench.rs`, `tests/bench_hygiene.rs`.

**Кроки:**
- [x] IDEAL-1 (`625-665`): лічити `admitted` per-thread; після join сумувати й `assert_eq!(admitted,
      iters_per_thread * concurrency)`; на виснаження 64 retry — `panic!("stale retry exhausted")`.
- [x] IDEAL-1: читати `stale` після join і або `assert!(stale < total/2)`, або друкувати в stderr
      з явним `BENCH WARN`; не залишати мертвий лічильник.
- [x] IDEAL-2 (`679-711`): те саме (admitted floor + panic на виснаженні).
- [x] IDEAL-3 (`730-757`): лічити успішні `send_critical` (`Ok`) і `Err`; після зупинки drain
      дочекатися повного дренування і `assert_eq!(received, sent_ok)`; `assert!(sent_ok > 0)`.
- [x] Усі `let _ = h.join();` (`663-665`, `709-711`, `755-757`, `808`) → `h.join().expect("bench worker panicked")`.
- [x] `tests/bench_hygiene.rs`:
      - заборонити `let _ = h.join()` (текстовий бан + whitelist, якщо є легітимні місця);
      - заборонити `let _ = black_box(sender.send` (відкинутий результат send);
      - додати `PER_SAMPLE_ALLOC` для `report_snapshot_4_lanes` і `expired_lanes_4_lanes` з поясненням,
        або перенести вимірювання так, щоб алокація була частиною заявленої метрики (і задокументувати);
      - прибрати wholesale-виняток IDEAL-3 (`286-288`): перевірити, що в IDEAL-3 немає жодного `HEAVY`
        конструктора, і ганяти загальний чек; якщо є — звузити виняток до конкретного ctor.
- [x] Перевірити `drain_sync_256_frames`: у README/коментарі явно «stream + scan per 256-frame sample»,
      щоб `Throughput::Elements(256)` не читався як чистий per-frame scan.
- [x] Прогнати `cargo bench --bench admission_bench -- --sample-size 10` (або вибірково) і зберегти
      нові числа; оновити README тільки тими числами, які реально виміряні в цьому прогоні.

**Тести:** `cargo test --test bench_hygiene`; самі бенчі компілюються й запускаються (smoke).

**Готово:** жоден contended-бенч не може опублікувати результат із нульовою кількістю допусків;
`bench_hygiene` ловить відкинутий join/send; README-числа мають свіжий артефакт.

**Коміт:** `fix(bench): fail fast on retry exhaustion and discarded send results`

**Дописи:**
- `iter_custom` у Criterion множить час на `iters`; якщо фактичних операцій менше — число занижене
  (виглядає швидше). Assert на кількість — єдиний спосіб це закрити.
- Для gateway «sent == received» може бути недосяжним, якщо drain зупиняється до повного спорожнення;
  тому після `draining=false` — `while rx.try_recv().is_some() { count += 1; }` (у коді вже є) і
  порівнювати після цього.
- Виконано: IDEAL-1..3 рахують `admitted`/`sent_ok`/`send_err`; retry-виснаження — `assert!`;
  `stale` після join або `BENCH WARN` у stderr; IDEAL-4 `popped == iters`; усі `join` → `.expect`.
  `bench_hygiene` отримав два нові тести (текстові бани join/send + `PER_SAMPLE_ALLOC`), виняток
  IDEAL-3 прибрано. Негативний прогін: доданий `let _ = stray.join();` валить гейт.
- Свіжий прогін 2026-10-25 (default samples, concurrency=6): enter 918 ns, scoped 911 ns,
  gateway 140 ns, ISR bridge 35.5 ns; таблиця README оновлена лише цими чотирма рядками.

---

### Етап 5 — Скрипти та CI (P1)

**Мета:** жоден SKIP/помилка парсингу/відсутній тег не мають виглядати як успіх; PS і sh мають
однакову силу; перформанс і Miri мають реальні лейни.

**Файли:** `scripts/check_compile_gates.{sh,ps1}`, `scripts/check_api_diff.{sh,ps1}`,
`scripts/stress_harness.{sh,ps1}`, `scripts/stress_multi_lineage.{py,ps1}`, `scripts/bench.{sh,ps1}`,
`.github/workflows/ci.yml`, `.github/workflows/stress.yml`, `tests/task_engine.rs`.

**Кроки:**
- [x] F-12: портувати в `.ps1` усі 15 лічильників і 3 функції (`enter_inner`, `enter_async_attempt`,
      `record_locked`); додати Windows CI-крок, який запускає `.ps1` і порівнює кількість перевірок із `.sh`.
      **Готово:** обидва flavors друкують `PARITY: gates=7 counters=15 functions=3 metaphors=5`; крок
      `Gate Flavor Parity (.sh vs .ps1)` на Windows запускає обидва і валить будь-яку розбіжність.
- [x] F-31: `check_compile_gates.sh` — `in_ci()` строго `CI=true|1`; у `printf | grep` перевіряти `rc`
      і падати на `rc > 1`/SIGPIPE, а не провалюватися в `OK`. **Готово:** `scan_hits` розрізняє rc 0/1/інші;
      негатив (підмінений `grep`, exit 2 на `format!`) дав `[3/7] ... FAIL: sync lock-path scan could not run`, exit 1.
- [x] F-30: `check_api_diff.sh` — якщо `git hash-object` повертає порожньо, це FAIL (lock-мутація невидима).
      **Готово:** перевірка є в обох flavors (`$LockBefore`/`$LockAfter` у `.ps1`).
- [x] F-33: `.ps1` — не друкувати `OK`, якщо `rc != 0` (FAIL baseline==HEAD). **Готово:** Gate 6 загорнутий
      у `else`, тож після FAIL baseline==HEAD жодного `OK` не друкується.
- [x] F-11: додати `scripts/check_release_tag.sh`: `Cargo.toml` version має дорівнювати останньому тегу
      (після `v`) **або** версія має бути явно позначена як «unreleased» у CHANGELOG; підключити в CI.
      Теги не створювати без наказу. **Готово:** крок у quality; поточне дерево —
      `WARN: Cargo.toml 0.6.8 is ahead of the latest tag v0.6.3 (documented in CHANGELOG, not tagged)`;
      негативи: 0.7.0 без секції → FAIL, 0.5.9 (позаду тега) → FAIL.
- [x] F-13 (sh): PID identity — повні ticks (`starttime`), без `/ CLK_TCK`; `LAST_KILLED_START_TICKS`
      замість секунд; macOS — задокументована секундна межа. JSON — реальний парсер (`python3 -c`)
      з fallback-помилкою, не regex. `add_skip` для fd-growth — FAIL у CI (`in_ci()`); `LAUNCH=0`
      у CI — FAIL, бо гейти не працювали. **Готово:** `read_json_int` через `python3` JSON; у CI
      потрібен `--launch` **або** `--external-supervisor` (тестовий раннер сам володіє супервізором).
- [x] F-29 (ps1): managed-path — точний image + creation time (як `-RawKill`), а не `-like "*name*"`;
      вирівняти kill-semantics і verdict з `stress_multi_lineage.py` (`killed==expected`, `armed_hold`,
      clamp fd-growth ≥ 0). **Готово:** `Get-StartTicks` + `GetFileNameWithoutExtension`; verdict
      містить `killed == requested` і `armed_hold`; growth затиснутий у ≥ 0.
- [x] F-32: `bench.sh` — перевіряти `rc` від `grep -c`; у `--github` без `GITHUB_STEP_SUMMARY` — FAIL
      (або явний WARN, але CI-режим має падати). **Готово:** rc>1 → FAIL; CI+`--github` без summary →
      exit 1, поза CI → WARN. Негативи: rc 2 → exit 1, rc 1 → exit 1 (порожній snapshot), CI=true → exit 1,
      CI=garbage → WARN/exit 0.
- [x] F-10: додати в `ci.yml` job `bench-full` на `schedule`/`workflow_dispatch`:
      `cargo bench --locked --bench admission_bench`, `bash scripts/bench.sh --skip-run`,
      upload artifacts `target/criterion/**` + `target/bench/snapshot.md`; код `bench.sh` не чіпати.
      **Готово:** job `bench-full`, `--locked`, `if-no-files-found: error`, `--github` (summary в CI).
- [x] F-14: додати лейн `miri-leaks` **без** `-Zmiri-ignore-leaks` (спочатку один, щоб зрозуміти обсяг);
      розширити список Miri-файлів до всіх, що компілюються без wall-clock/sleep; для виключених
      (`channel.rs:13`, `task_engine.rs:271`) — задокументувати причину в CI-коментарі.
      **Готово:** lib + 14 core suites; локально lib 24 passed, усі suites green (найдовший
      `bulkhead_capacity` 107 s). Виключення: `channel.rs:13` (sleep-throttle), `task_engine.rs:271`
      (TTL-годинник), supervisor-сюїти (реальні процеси).
- [x] F-17: `FakeWake` — прибрати `#[allow(dead_code)]`, використати `self.0` у `wake`; додати
      `#[cfg(miri)] assert!(!w1.will_wake(&w2))` для різних `Arc` (з застереженням, що `will_wake`
      може over-approximate; це hardening, не гарантований tripwire). **Готово:** поле читається в
      обох `wake`-методах; тест `fake_waker_identity_matches_allocation` (обидва `Arc` живі, адреси
      не аліасяться); suite 400 → 401.
- [x] F-34: додати PR/push-лейн (зменшені counts) для `supervisor_termination` і Windows raw-kill,
      або явно задокументувати, що вони тижневі, і додати `workflow_dispatch`-чеклист.
      **Готово:** job `termination-smoke` (ubuntu + windows, `STRESS_TARGET_KILLS=25`,
      `cargo test --release --locked ... -- --ignored`) на кожен push/PR.
- [x] Перевірити `--locked` на всіх нових кроках; `fmt`/`semver-checks` — задокументовані винятки.
      **Готово:** усі нові кроки `--locked`; `cargo-semver-checks` не має такого прапорця (lock
      хешується до/після).

**Тести:** локально `bash scripts/check_compile_gates.sh`, `pwsh -File scripts/check_compile_gates.ps1`
(якщо є pwsh), `bash scripts/check_api_diff.sh`, `bash scripts/bench.sh --skip-run`.
Негативні прогони: штучно зламати JSON/тег/лічильник і переконатися, що скрипт падає.

**Готово:** sh/PS дають однаковий verdict на одному дереві; SKIP у CI → FAIL; PID-гейт розрізняє
двох воркерів у межах секунди (Linux); `bench-full` архівує артефакти; Miri-лейн без `ignore-leaks`
або явний перелік знайдених витоків із рішенням.

**Стан 2026-10-25:** обидва flavors — `PARITY: gates=7 counters=15 functions=3 metaphors=5`, exit 0
(локально 1 SKIP: semver-checks не встановлено). `miri-leaks` локально зелений (lib 24 + 14 suites).
Suite 401 passed / 0 failed, clippy чистий, badge-гейт OK. Негативи: grep exit 2 у Gate 3 → FAIL;
bench `grep` rc 2 → FAIL; `--github` у CI без summary → exit 1; harness без `--launch`/`--external-supervisor`
у CI → exit 1; release-гейт 0.7.0/0.5.9 → FAIL.

**Коміт:** `fix(ci): close skip/parse/parity holes and add full bench artifact lane`

**Дописи:**
- PS 5.1: файли лишаються ASCII-only; ем-даш у коментарі ламає парсинг у cp1252 — перевіряти
  після кожної правки.
- Miri без `ignore-leaks` може впасти на існуючих тестах; це не привід повертати прапорець —
  спочатку тріаж: справжній витік vs навмисний `Box::leak` у тесті.
- Повний bench на CI-ранері шумний; тому він **не** гейтить PR, лише архівує й будує тренд.
- `supervisor_termination` сам запускає супервізор, тож harness у CI отримує `--external-supervisor`;
  без цього прапорця CI-правило «або `--launch`, або явний external» валить лейн (перевірено).
- Windows parity-крок запускає **обидва** flavors в одному job: `.sh` у Git Bash, `.ps1` у PS 5.1;
  порівнюється лише рядок `PARITY:`, тому кількісний дрейф (новий лічильник в одному flavor) червоний.
- `scripts/stress_harness.ps1` не має CI-правила для `-Launch`: тестовий раннер передає зовнішній
  супервізор і на Windows, а PS-версія не має окремого прапорця external.

---

### Етап 6 — Документація (P2)

**Мета:** кожне твердження в README/USER_GUIDE або істинне, або має дату/джерело.

**Файли:** `README.md`, `docs/USER_GUIDE.md`, `CHANGELOG.md`, `docs/INVARIANTS.md` (публічний).

**Кроки:**
- [x] F-18: `USER_GUIDE.md:1788` — `lane.attach(..)` / `lane.note_death(..)`; звірити з прикладом
      `USER_GUIDE.md:1123-1130`.
- [x] F-15: `USER_GUIDE.md:1787` — розділити рядок: core = `SnubScanner::push` + `ProcessToken::*`;
      `feed`/`finish`/`write_to` — окремий рядок `std`, з приміткою «requires `std::io::Write`; для UART/DMA
      потрібен власний адаптер».
- [x] F-04: виправити «no allocation» (див. 1.4).
- [x] F-19: назви бенчів — додати `_threads_{N}` у README або явно писати «family `contended_node_enter_threads_*`»;
      `classify_stall` → `classify_stall_ceiling` (або позначити, що це API-ім'я).
- [x] F-20: біля таблиці латентності — provenance-блок: дата, команда, посилання на артефакт
      (або «historical, see CI artifact»); бейдж — команда перевірки.
- [x] F-21: додати в API-таблицю публічні аксесори:
      `PoolConfig::with_memory/with_eof_grace`, `PoolLane::respawn_delay_ms/next_epoch`,
      `WorkerPool::total_rescued`, `PoolReport::deadlocked/final_generations`,
      `ExecutionEnvelope::{off,is_inside,has_underflow,has_overflow,is_off,time_verdict,payload_verdict}`,
      `FastPrng`, `HarnessReport::mean_us`, `ProfileHarness::{samples,burst_len}`, `ProcessToken::commit`.
- [x] F-20/ratios: додати в README короткі посилання на rationale (50 MB trap, cgroup/fadvise,
      `max+1`, «Level Budget, Not Time Budget») — зараз вони є лише в USER_GUIDE.
- [x] Перевірити, що жоден рядок не обіцяє bare-metal для std-API; grep `core`/`bare metal`/`no_std`
      по таблицях.
- [x] `CHANGELOG.md`: додати секцію для наступної версії з усіма поведінковими змінами етапів 1–4
      (без version bump, якщо реліз не замовлено).

**Тести:** `cargo test --doc` (26 passed); `grep`-аудит протиріч; усі 24 згадані аксесори знайдено в `src/`.

**Готово:** жодного неіснуючого методу/бенча в docs; «no allocation» відсутнє там, де є `Vec`;
кожне число має джерело; усі публічні типи 0.6.x мають хоч один рядок у таблиці.

**Коміт:** `docs: correct pool API receivers, core/std split, allocations, bench names, accessors`

**Дописи:**
- Локальні docs не згадувати і не комітити; публічні — README/USER_GUIDE/INVARIANTS/CHANGELOG.
- Не вигадувати нові числа: якщо бенч не прогнаний — писати «historical» і дату, а не «re-measured».
- `CHANGELOG.md` секція `## Unreleased`: hook-паніки, порядок reset сканера, lock hygiene, clock поза
  замком, `feeds_after_eof`, чесні tests/benches, CI-гейти. Версія в `Cargo.toml` не змінена (0.6.8),
  гейт `check_release_tag.sh` дає OK «ahead of v0.6.3 and marked Unreleased».
- Виправлено два хибні bare-metal твердження: async engine тепер явно `std`-gated у USER_GUIDE, а
  README-сніпет `PipeDrain::manual_with_clock`/`feed` позначений як `std`-транспорт поверх core-сканера.
- Анкори rationale перевірені проти реальних заголовків USER_GUIDE (`4.6c`, `Why a naive serialization…`,
  `cgroup v2…`, `Fixed in 0.6.4…`).

---

### Етап 7 — Покриття та дрібні ризики (P2/P3)

**Мета:** закрити тестові діри, які не є критичними, але дозволяють регресіям проходити.

**Файли:** `tests/async_primitives.rs`, `tests/conduit.rs`, `tests/guard.rs`, `tests/erased.rs`,
`examples/12_isr_bridge.rs`, `src/node.rs`, `src/pool/lane.rs`, `src/channel.rs`.

**Кроки:**
- [x] F-28: тест `future_dropped_after_settle_pop_hands_the_slot_to_the_next_waiter` (handoff з
      `async_engine/entry.rs:13-16`; settle будить рівно одного, cancel передає слот наступному).
- [x] F-28: тест `half_open_async_probe_cancelled_in_queue_does_not_wedge_the_breaker` (async probe
      паркується, скасовується, новий probe закриває breaker).
- [x] F-28: тест `async_admission_recovers_from_a_poisoned_ledger` (unit-тест у `src/node.rs`:
      отруєний ledger, `poison_recoveries >= 1`, `commit`, консервація).
- [x] F-26: перейменувати статики в `tests/guard.rs` (зроблено в Етапі 3:
      `ROLLBACKS_ON_COMMIT` / `ROLLBACKS_ON_DROP` / `ROLLBACKS_ON_ABORT`).
- [x] F-27: `examples/12_isr_bridge.rs` — після abort у cycle 3 assert, що `laser_on == false`
      (приклад прогнано: `[Guard] laser off`, assert зелений).
- [x] F-23: синтетичний фід перейменовано в `synthetic_anomaly_feed_triggers_preempt_storm_verdict`
      (Етап 3); справжнє preemption-покриття живе в `tests/priority_preemption.rs`.
- [x] F-35: задокументовано TTL re-stamp у USER_GUIDE (`task_ttl` bullet: перший park, без re-stamp,
      settle-wake edge = залишок lease); тест `spurious_repolls_do_not_reset_the_ttl_clock` уже покриває
      межу одного park.
- [x] F-36/F-37/F-38/F-39: рішення в коді —
      `next_sequence` Relaxed пояснено (unique ticket, не wall-clock);
      `LaneState` pid/drain задокументовано (panic-point між двома store відсутній);
      `channel.rs` depth переведено на Relaxed (advisory gauge, авторитет — `try_send`; latch
      лишається Release/Acquire);
      два SeqCst fences в `ErasedTransaction` видалено (замок ledger і є ordering edge).
- [x] F-37: атомарну пару не вводив — comment фіксує умову, за якої це знадобиться.

**Тести:** `cargo test --lib` (25), `cargo test --test async_primitives` (17), `cargo run --example 12_isr_bridge`,
повний suite 378 passed (+26 doctests = 404).

**Готово:** нові тести зелені; приклад має post-abort assert; P3-пункти мають явне рішення
(фікс або документований виняток).

**Коміт:** `test(async): cover settle handoff, half-open cancel, poisoned node path`

**Дописи:**
- Poison-тест не потребує публічного API: unit-модуль у `src/node.rs` бачить `inner` і труїть
  ledger через `catch_unwind` під `MutexGuard`. Waker створено через `std::task::Wake` (без `unsafe`),
  тож `#![forbid(unsafe_code)]` лишається чинним і для тестів.
- `Relaxed` на gateway depth — не послаблення: `fetch_add`/`fetch_sub` і раніше були Relaxed, тож
  Acquire-читач ні з чим не парився; тепер це чесно задокументовано, а latch (`throttled`) зберігає
  Release/Acquire.
- Видалені fences нічого не впорядковували: наступний виклик бере `Mutex` ledger, який сам дає
  acquire/release edge; окремий SeqCst fence був no-op.

---

### Етап 8 — Реліз (P2, тільки за наказом)

**Мета:** зафіксувати виправлення у версії та CHANGELOG, не порушивши semver.

**Кроки:**
- [x] Переконатися, що Етапи 0–7 закриті; suite/gates/clippy/fmt зелені.
- [x] `Cargo.toml` version → `0.6.9` (або що скаже власник); `CHANGELOG.md` — повний список.
- [x] `bash scripts/check_release_tag.sh` — `WARN: Cargo.toml 0.6.9 is ahead of the latest tag v0.6.3`
      (exit 0; версія задокументована, тег — окремо).
- [x] **Тег — тільки за окремим наказом.** Наказу не було: `v0.6.9` не створено і не запушено;
      останній тег лишається `v0.6.3`.
- [x] Commit + push (за standing preference), з measured before→after для кожного перф-пункту.

**Готово:** version і CHANGELOG узгоджені; CI зелений; тег або створений за наказом, або явно
залишений на потім.

**Коміт:** `release: 0.6.9 — audit closeout`

**Дописи:**
- Гейт `check_release_tag.sh` дає WARN, не FAIL: версія попереду тегу, але секція
  `## 0.6.9 — 2026-10-25` у CHANGELOG існує. FAIL був би лише за відсутньої секції.
- Локальні гейти: 6 OK, Gate 6 SKIP — `cargo-semver-checks` локально не встановлено, CI його ставить;
  відсутність гейта в CI = FAIL, тож SKIP не може проскочити в реліз.
- Тег не створювався: це окремий наказ. Версія в маніфесті від цього не залежить — semver-гейт
  і далі дериться від `v0.6.3`, і саме тому тег на кожен реліз є частиною процесу.

---

## 3. Закрито без змін (перевірено — не є дефектом)

- [x] **`tests/task_engine.rs:264` int→ptr cast — відсутній.** `Waker::from(Arc::new(FakeWake(id)))`;
      жодного `as *mut`/`as *const`/`expose_addr`/`transmute` у `src tests benches examples`
      (єдиний збіг — коментар `src/supervisor.rs:571`). Strict Provenance задоволений.
- [x] **Отруєння замків.** Немає `.lock().unwrap()/.expect` у `src`; `NodeInner`, `cancel_reason`,
      `draft`, `wakers`, `state(RwLock)`, `GatewayBooks`, `LaneState`, `DrainInner` — усі через
      `into_inner` + `hard_clamp_slots`.
- [x] **Saturating-арифметика та відсутність `unsafe`.** `#![forbid(unsafe_code)]`; Gate 2.
- [x] **32-bit без 64-bit atomics.** `AtomicU64` лише в std-модулях; core `clock.rs` має
      `cfg(target_has_atomic = "64")` + `AtomicU32` fallback; `NodeName::Owned` під ptr-atomics.
- [x] **Розмірні контракти.** 512 B (`src/diagnostics.rs:27-29`), 64 B (`src/supervisor.rs:524,533,537`),
      44/40/28 B (`src/drain.rs:19,28,89`), lane mask (`src/pool/task.rs`, `src/pool/mod.rs`).
- [x] **`bench.sh` fail-closed.** Немає `target/criterion` → exit 1 (`:51`); нема python3 → exit 1 (`:69`);
      порожній snapshot → exit 1 (`:151`).
- [x] **Memory orderings — коректні.** `memory_pressure` Release/Acquire (`node.rs:1849`, `2973`, `3544`);
      ISR SPSC — cell Relaxed + head Release/Acquire; решта Relaxed — телеметрія.
- [x] **Async cancel (select! drop).** Немає stale waker/leased slot/probe leak: `entry.rs:80-82`,
      `node.rs:3668-3671`, `3826`; probe чиститься до кожного `Pending`/`Err` (`3640,3665,3774,3805`).
- [x] **`ResumeFuture`.** Identity `Arc<AtomicBool>` лише для `ptr_eq`; Drop deregisters; lost-wakeup
      закритий порядком замків; TTL немає (N/A).
- [x] **Fake rollbacks у `examples/`.** `01`, `02`, `03`, `11`, `12`, `13` мають реальний стан/assert
      (єдине — cycle 3 у `12`, винесено в F-27).
- [x] **Локальні docs.** Усі сім — gitignored/untracked; жоден тест їх не читає; `tests/doc_sync.rs`
      читає лише `src/**/*.rs`.
- [x] **`#[ignore]` мають CI-лейни.** `stress_breaker_churn`, `supervisor_pool_stress*` — `stress.yml:107-114`;
      `supervisor_termination` — `stress.yml:210-227` (тижнево; див. F-34).
- [x] **`--locked`.** Усі build/test-виклики locked; винятки `fmt`/`semver-checks` компенсовані
      lockfile-хешем (діра з git — F-30).

---

## 4. Порядок комітів

1. `chore(ci): add reproducible test-count gate and record audit baseline` — Етап 0.
2. `fix(core): panic-safe probe hooks, scanner reset order, publish_fault lock hygiene` — Етап 1.
3. `fix(concurrency): keep waker drops and clock reads outside mutexes` — Етап 2.
4. `test(conservation): pin full admission law and positive volume floors` — Етап 3.
5. `fix(bench): fail fast on retry exhaustion and discarded send results` — Етап 4.
6. `fix(ci): close skip/parse/parity holes and add full bench artifact lane` — Етап 5.
7. `docs: correct pool API receivers, core/std split, allocations, bench names, accessors` — Етап 6.
8. `test(async): cover settle handoff, half-open cancel, poisoned node path` — Етап 7.
9. `release: 0.6.9 — audit closeout` — Етап 8, тільки за наказом.

Після кожного коміту: `cargo fmt --check`, `cargo clippy -D warnings`, suite, `scripts/check_compile_gates.sh`.
Push — одразу після зеленого прогону (standing preference), без rewrite історії.

---

## 5. Ризики, відкат, критерії зупинки

- **Ризик R1: двофазний `retain` у node.** Може внести race/lost waiter. Якщо тести на cancel/evict
  нестабільні — відкат до `retain` під замком + явний доккоментар-виняток. Не залишати «напівфікс».
- **Ризик R2: `catch_unwind` навколо hook'ів.** Може змінити поведінку, якщо hook навмисно панікує
  для керування потоком (не підтримується). Контракт: паніка = баг користувача, але вузол лишається живим.
- **Ризик R3: посилення conservation-asserts.** Може виявити реальні десинхронізації, які раніше
  маскувалися. Це мета, а не привід послаблювати assert; фіксувати root cause.
- **Ризик R4: Miri без `ignore-leaks`.** Може впасти на навмисних тестових витоках; тріаж, не повернення прапорця.
- **Ризик R5: PS-гейт.** PS 5.1 + cp1252: тільки ASCII; після кожної правки — прогін `pwsh`/`powershell`.
- **Ризик R6: повний bench на CI.** Шумний і повільний; не гейтити PR, лише архівувати.
- **Зупинка:** якщо будь-який етап вимагає зміни публічного API з порушенням semver — зупинитися,
  описати варіанти (deprecation/additive) і узгодити. Не ламати API мовчки.
- **Відкат:** кожен етап — окремий коміт; відкат `git revert <sha>` без rewrite. PLAN.md оновлювати
  статусами після кожного етапу.

---

## 6. Журнал виконання

| Дата | Етап | Результат | Коміт |
|---|---|---|---|
| 2026-10-25 | Аудит | 39 знахідок, 3 секції; файли не змінювалися | — |
| 2026-10-25 | 0 | PLAN.md створено; baseline 394 зафіксовано | — |
| 2026-10-25 | 1 | F-01..F-04 виправлено; +3 регресії; suite 394 -> 397 | `fix(core): panic-safe probe hooks, scanner reset order, publish_fault lock hygiene` (3288b34) |
| 2026-10-25 | 0 | `check_test_count.sh` + CI-гейт; бейдж 397 з provenance | `chore(ci): add reproducible test-count gate and record audit baseline` (d150d64) |
| 2026-10-25 | 2 | F-07/F-08/F-09/F-16; waker drops і clock поза замками; feed-after-EOF лічильник; suite 397 -> 399 | `fix(concurrency): keep waker drops and clock reads outside mutexes` (7481e9a) |
| 2026-10-25 | 3 | F-05/F-22/F-23/F-24/F-25/F-26; повний 7-доданковий закон, floor-и, справжній rollback-свідок; suite 399 | `test(conservation): pin full admission law and positive volume floors` (dc12cd4) |
| 2026-10-25 | 4 | F-06: contended-бенчі падають на виснаженні retry/нулі допусків; `bench_hygiene` ловить join/send; README contended-рядки переміряно | `fix(bench): fail fast on retry exhaustion and discarded send results` (7c9e6bf) |
| 2026-10-25 | 5 | F-10..F-14/F-17/F-29..F-34: PARITY-фінгерпринт, строгий CI, release-гейт, ticks, `bench-full`, `miri-leaks`, `termination-smoke`; suite 400 -> 401 | `fix(ci): close skip/parse/parity holes and add full bench artifact lane` (7bf69ee) |
| 2026-10-25 | 6 | F-15/F-18/F-19/F-20/F-21: core/std split, `lane.*` receivers, чесний `Vec` у `report()`, бенч-ID, provenance + artifact, 24 аксесори, README rationale-анкори, CHANGELOG `Unreleased`; doctests 26; suite 401 | `docs: correct pool API receivers, core/std split, allocations, bench names, accessors` (e1252fb) |
| 2026-10-25 | 7 | F-27/F-28/F-35..F-39: 3 нові тести (handoff, half-open cancel, poisoned ledger), post-abort assert, TTL/ordering/lane/fence рішення; suite 404 (378+26 doctests) | `test(async): cover settle handoff, half-open cancel, poisoned node path` (67f26b8) |
| 2026-10-25 | 8 | release 0.6.9: `Cargo.toml`+lock+CHANGELOG; гейт `WARN` (документовано, не теговано); fmt/clippy/suite 404 зелені, 6 gates OK (Gate 6 SKIP локально); тег `v0.6.9` не створено — окремий наказ | `release: 0.6.9 — audit closeout` (784453c) |
|  |  |  |  |
