#!/usr/bin/env python3
"""Multi-lineage process-death storm harness for Linux and macOS.

N independent supervisors, one lineage each: own worker, pid file, pipe and
report. Isolation needs no library changes: examples/process_termination_loop.rs
writes the worker pid to the RELATIVE path ``target/supervised_worker.pid``, so
each lane runs in its own working directory.

The single-lineage release gate stays frozen in ``stress_harness.ps1`` (Windows)
and ``stress_multi_lineage.ps1`` (Windows, raw Win32 kill). This script is the
POSIX equivalent of the latter: SIGKILL instead of TerminateProcess, process
identity from ``/proc/<pid>/stat`` starttime (Linux) or ``ps -o lstart``
(macOS), so a recycled pid is never mistaken for the live worker.

Every reap is classified from the supervisor report: true ambush (armed token
at death), clean escape (commit seen) or stillborn (neither). ``--armed-hold``
parks the worker armed and publishes its pid only after the arm frame, so
``--min-ambush-pct 70`` proves armed workers were killed, not startup noise.

Exit code 0 = PASS, 1 = FAIL.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import signal
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

IS_LINUX = sys.platform.startswith("linux")


def resolve(repo: Path, value: str) -> Path:
    p = Path(value)
    return p if p.is_absolute() else repo / p


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, math.ceil(pct / 100.0 * len(ordered)) - 1))
    return ordered[idx]


def read_pid(path: Path) -> int | None:
    try:
        raw = path.read_text().strip()
        pid = int(raw)
        return pid if pid > 4 else None
    except (OSError, ValueError):
        return None


def proc_identity(pid: int) -> tuple[str, str] | None:
    """Return (start identity, image basename) or None when the pid is gone."""
    if IS_LINUX:
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
            close = stat.rfind(")")
            fields = stat[close + 2 :].split()
            start = fields[19]  # field 22 (starttime), zero-based from state=field 3
            image = Path(os.readlink(f"/proc/{pid}/exe")).name
            return start, image
        except OSError:
            return None
    try:
        # -ww disables the 16-column comm truncation; without it a long exe
        # path is cut and the image check would reject every live worker.
        out = subprocess.run(
            ["ps", "-ww", "-o", "lstart=,comm=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    line = out.stdout.strip()
    if out.returncode != 0 or not line:
        return None
    parts = line.rsplit(None, 1)
    if len(parts) != 2:
        return None
    return parts[0].strip(), Path(parts[1]).name


def proc_rss_mb(pid: int) -> float | None:
    if IS_LINUX:
        try:
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024.0
        except (OSError, ValueError, IndexError):
            return None
        return None
    try:
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=2,
        )
        return int(out.stdout.strip()) / 1024.0 if out.returncode == 0 else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def proc_fds(pid: int) -> int | None:
    if not IS_LINUX:
        return None
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return None


class Sample:
    def __init__(self) -> None:
        self.first: float | None = None
        self.last: float | None = None
        self.peak: float = 0.0
        self.count: int = 0

    def note(self, value: float | None) -> None:
        if value is None:
            return
        if self.first is None:
            self.first = value
        self.last = value
        self.peak = max(self.peak, value)
        self.count += 1

    def growth(self) -> float:
        if self.first is None or self.last is None:
            return 0.0
        return max(0.0, self.last - self.first)

    def as_dict(self) -> dict:
        return {
            "first": round(self.first, 3) if self.first is not None else None,
            "peak": round(self.peak, 3),
            "last": round(self.last, 3) if self.last is not None else None,
            "growth": round(self.growth(), 3),
            "samples": self.count,
        }


class Lane:
    def __init__(self, index: int, directory: Path, report: Path) -> None:
        self.index = index
        self.dir = directory
        self.report_path = report
        self.sup: subprocess.Popen | None = None
        self.sup_exit: int | None = None
        self.killed = 0
        self.stale = 0
        self.duplicates = 0
        self.retries = 0
        self.stalled = False
        self.error: str | None = None
        self.latencies_us: list[float] = []
        self.last_killed: tuple[int, str] | None = None
        self.report: dict | None = None
        self.sup_rss = Sample()
        self.worker_rss = Sample()
        self.sup_fds = Sample()
        self.lock = threading.Lock()


def spawn_supervisor(args: argparse.Namespace, repo: Path, lane: Lane) -> None:
    exe = repo / "target" / args.profile / "examples" / args.target
    lane.dir.mkdir(parents=True, exist_ok=True)
    sup_args = [
        str(exe),
        "--target-kills",
        str(args.kills_per_line),
        "--report-path",
        str(lane.report_path),
    ]
    if args.aggressive:
        sup_args.append("--spin")
    if args.armed_hold:
        sup_args.append("--armed-hold")
    out = open(lane.dir / "supervisor.out", "w", encoding="utf-8")
    err = open(lane.dir / "supervisor.err", "w", encoding="utf-8")
    lane.sup = subprocess.Popen(sup_args, cwd=lane.dir, stdout=out, stderr=err)
    out.close()
    err.close()


def killer(args: argparse.Namespace, lane: Lane, supervisors: set[int], stop: threading.Event) -> None:
    pid_file = lane.dir / "target" / "supervised_worker.pid"
    last_progress = time.monotonic()
    identity_cache: dict[int, tuple[str, str] | None] = {}

    try:
        while not stop.is_set() and lane.killed < args.kills_per_line:
            if lane.sup is not None and lane.sup.poll() is not None:
                lane.error = f"supervisor exited before {args.kills_per_line} kills"
                return

            pid = read_pid(pid_file)
            if pid is None or pid in supervisors or pid == os.getpid():
                pid = None

            if pid is not None:
                ident = identity_cache.get(pid)
                if ident is None:
                    ident = proc_identity(pid)
                    if ident is not None:
                        identity_cache[pid] = ident
                if ident is None or ident[1] != args.target:
                    # Gone, torn read or a foreign image. Never blacklist the
                    # pid: a live child must stay retryable.
                    lane.stale += 1
                    pid = None
                elif lane.last_killed == (pid, ident[0]):
                    lane.duplicates += 1
                    pid = None
                else:
                    try:
                        started = time.perf_counter_ns()
                        os.kill(pid, signal.SIGKILL)
                        elapsed_us = (time.perf_counter_ns() - started) / 1000.0
                    except ProcessLookupError:
                        identity_cache.pop(pid, None)
                        lane.stale += 1
                        pid = None
                    except PermissionError:
                        lane.retries += 1
                        pid = None
                    else:
                        with lane.lock:
                            lane.killed += 1
                            lane.latencies_us.append(elapsed_us)
                        lane.last_killed = (pid, ident[0])
                        # Drop the cached identity: the pid may be recycled
                        # before the next iteration and must be re-verified.
                        identity_cache.pop(pid, None)
                        last_progress = time.monotonic()
                        continue

            if args.poll_ms > 0:
                time.sleep(args.poll_ms / 1000.0)
            else:
                time.sleep(0)

            if time.monotonic() - last_progress > args.stall_sec:
                lane.stalled = True
                lane.error = f"no kill progress for {args.stall_sec}s"
                return
    except Exception as exc:  # noqa: BLE001 - killer must report, not vanish
        lane.error = f"{type(exc).__name__}: {exc}"
        return

    if lane.killed < args.kills_per_line and not stop.is_set():
        lane.stalled = True
        lane.error = "killer exited before reaching kills-per-line"


def monitor(args: argparse.Namespace, lanes: list[Lane], supervisors: set[int], stop: threading.Event) -> None:
    started = time.monotonic()
    last_line = 0.0
    last_sample = 0.0
    while not stop.is_set():
        now = time.monotonic()
        # RSS reads spawn `ps` on macOS; sample resources at 10 Hz, print at 2 Hz.
        if now - last_sample >= 0.1:
            last_sample = now
            for lane in lanes:
                if lane.sup and lane.sup.poll() is None:
                    lane.sup_rss.note(proc_rss_mb(lane.sup.pid))
                    lane.sup_fds.note(proc_fds(lane.sup.pid))
                worker = read_pid(lane.dir / "target" / "supervised_worker.pid")
                if worker and worker not in supervisors:
                    # Same identity gate as the killer: a recycled pid must
                    # never pollute worker resource samples.
                    ident = proc_identity(worker)
                    if ident is not None and ident[1] == args.target:
                        lane.worker_rss.note(proc_rss_mb(worker))
        if now - last_line >= 0.5:
            last_line = now
            total = sum(lane.killed for lane in lanes)
            rate = total / max(now - started, 0.001)
            live = sum(1 for lane in lanes if lane.sup and lane.sup.poll() is None)
            print(
                f"[STORM] killed={total} rate={rate:.1f}/s lanes={len(lanes)} "
                f"supervisors_alive={live}",
                flush=True,
            )
        time.sleep(0.05)


def kill_lane_worker(lane: "Lane", supervisors: set[int], target: str) -> None:
    """Kill one lane's live worker, identity-gated (same rule as cleanup)."""
    worker = read_pid(lane.dir / "target" / "supervised_worker.pid")
    if worker is None or worker in supervisors:
        return
    ident = proc_identity(worker)
    if ident is None or ident[1] != target:
        return
    try:
        os.kill(worker, signal.SIGKILL)
    except OSError:
        pass


def cleanup(lanes: list[Lane], supervisors: set[int], target: str) -> None:
    """Abort path: never leave spinning supervisors or workers behind."""
    for lane in lanes:
        if lane.sup is not None and lane.sup.poll() is None:
            print(f"[CLEANUP] lane {lane.index} killing supervisor pid={lane.sup.pid}", flush=True)
            lane.sup.kill()
            try:
                lane.sup.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
    for lane in lanes:
        kill_lane_worker(lane, supervisors, target)


def run(args: argparse.Namespace, repo: Path) -> int:
    if sys.platform == "win32":
        print("Windows uses scripts/stress_multi_lineage.ps1 (raw Win32 kill path).", file=sys.stderr)
        return 1

    exe = repo / "target" / args.profile / "examples" / args.target
    if args.build or not exe.exists():
        cmd = ["cargo", "build", "--example", args.target]
        if args.profile == "release":
            cmd.insert(2, "--release")
        print(f"[BUILD] {' '.join(cmd)}", flush=True)
        if subprocess.run(cmd, cwd=repo).returncode != 0:
            print("cargo build failed", file=sys.stderr)
            return 1
    if not exe.exists():
        print(f"supervisor binary not found: {exe}", file=sys.stderr)
        return 1

    lane_root = resolve(repo, args.lane_root).resolve()
    target_root = (repo / "target").resolve()
    if target_root not in lane_root.parents:
        print(f"refusing to clean lane root outside target/: {lane_root}", file=sys.stderr)
        return 1
    if lane_root.exists():
        shutil.rmtree(lane_root)
    lane_root.mkdir(parents=True)

    lanes: list[Lane] = []
    for i in range(args.lines):
        directory = lane_root / f"lane_{i}"
        lanes.append(Lane(i, directory, directory / "termination_report.json"))

    print(
        f"[CONFIG] lines={args.lines} kills_per_line={args.kills_per_line} "
        f"armed_hold={args.armed_hold} aggressive={args.aggressive} "
        f"profile={args.profile} platform={sys.platform}",
        flush=True,
    )

    supervisors: set[int] = set()
    duration_s = 0.0
    finalized = False
    try:
        for lane in lanes:
            try:
                spawn_supervisor(args, repo, lane)
            except OSError as exc:
                print(f"lane {lane.index} spawn failed: {exc}", file=sys.stderr)
                return 1
            supervisors.add(lane.sup.pid)

        deadline = time.monotonic() + 15
        missing: list[int] = []
        while time.monotonic() < deadline:
            missing = [
                lane.index
                for lane in lanes
                if not (lane.dir / "target" / "supervised_worker.pid").exists()
            ]
            if not missing:
                break
            for lane in lanes:
                if lane.sup and lane.sup.poll() is not None:
                    print(
                        f"lane {lane.index} supervisor exited before publishing a pid "
                        f"(exit={lane.sup.returncode})",
                        file=sys.stderr,
                    )
                    return 1
            time.sleep(0.02)
        else:
            print(f"lanes published no worker pid within 15 s: {missing}", file=sys.stderr)
            return 1

        stop = threading.Event()
        killers = [
            threading.Thread(target=killer, args=(args, lane, supervisors, stop), daemon=True)
            for lane in lanes
        ]
        monitor_thread = threading.Thread(
            target=monitor, args=(args, lanes, supervisors, stop), daemon=True
        )
        for thread in killers:
            thread.start()
        monitor_thread.start()

        storm_started = time.monotonic()
        for lane, thread in zip(lanes, killers):
            thread.join(timeout=args.kills_per_line * 0.1 + 30)
            if thread.is_alive():
                lane.stalled = True
                lane.error = lane.error or "killer thread did not finish"
        duration_s = max(time.monotonic() - storm_started, 0.001)
        stop.set()
        monitor_thread.join(timeout=5)

        if any(lane.sup and lane.sup.poll() is None for lane in lanes):
            print("[WAIT] supervisors finishing reports", flush=True)
        for lane in lanes:
            if lane.sup is None:
                continue
            try:
                lane.sup_exit = lane.sup.wait(timeout=args.supervisor_wait)
            except subprocess.TimeoutExpired:
                # Grace before the hard kill: SIGTERM (POSIX) lets a
                # slow-but-alive supervisor finish and write its report;
                # only a wedged one reaches the SIGKILL below (its
                # missing report is then expected and reported, not a
                # silent gate hole). Windows terminate() is already
                # immediate — the kill branch stays unreachable there.
                lane.sup.terminate()
                try:
                    lane.sup.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    lane.sup.kill()
                    lane.sup.wait(timeout=5)
                lane.sup_exit = lane.sup.returncode
                lane.error = lane.error or "supervisor did not exit (terminated after grace)"
                # A hard-killed supervisor cannot reap its own child: the
                # worker would spin forever past `finalized=True`. Kill it
                # here, identity-gated (same rule as cleanup()).
                kill_lane_worker(lane, supervisors, args.target)

        for lane in lanes:
            try:
                lane.report = json.loads(lane.report_path.read_text())
            except (OSError, json.JSONDecodeError):
                lane.report = None
        finalized = True
    finally:
        if not finalized:
            cleanup(lanes, supervisors, args.target)

    checks: list[tuple[str, bool, str]] = []
    total_killed = sum(lane.killed for lane in lanes)
    total_reaped = sum(int((lane.report or {}).get("total_reaped", 0)) for lane in lanes)
    total_ambush = sum(int((lane.report or {}).get("ambush_kills", 0)) for lane in lanes)
    total_escape = sum(int((lane.report or {}).get("escape_kills", 0)) for lane in lanes)
    total_stillborn = sum(int((lane.report or {}).get("stillborn_kills", 0)) for lane in lanes)
    total_classes = total_ambush + total_escape + total_stillborn
    expected = args.lines * args.kills_per_line
    min_ambush = args.min_ambush_pct
    if min_ambush < 0:
        min_ambush = 70 if args.armed_hold else 0

    for lane in lanes:
        tag = f"lane{lane.index}"
        report = lane.report or {}
        reaped = int(report.get("total_reaped", 0))
        ambush = int(report.get("ambush_kills", 0))
        escape = int(report.get("escape_kills", 0))
        stillborn = int(report.get("stillborn_kills", 0))
        ambush_pct = 100.0 * ambush / reaped if reaped else 0.0
        checks.append((f"{tag} report present", lane.report is not None, ""))
        checks.append((f"{tag} reaped == kills", reaped == args.kills_per_line, f"{reaped}"))
        checks.append(
            (
                f"{tag} ambush+escape+stillborn == reaped",
                ambush + escape + stillborn == reaped,
                f"{ambush}+{escape}+{stillborn}",
            )
        )
        checks.append(
            (f"{tag} armed_hold matches", bool(report.get("armed_hold", False)) == args.armed_hold, "")
        )
        checks.append(
            (f"{tag} final gen == kills+1", int(report.get("final_generation", 0)) == args.kills_per_line + 1, "")
        )
        checks.append((f"{tag} no pipe deadlock", not bool(report.get("deadlock", True)), ""))
        checks.append((f"{tag} supervisor exit 0", lane.sup_exit == 0, str(lane.sup_exit)))
        checks.append(
            (f"{tag} killer no error/stall", lane.error is None and not lane.stalled, lane.error or "")
        )
        checks.append(
            (f"{tag} ambush >= {min_ambush:.0f}%", ambush_pct + 1e-9 >= min_ambush, f"{ambush_pct:.1f}%")
        )
        checks.append(
            (
                f"{tag} rss growth <= {args.max_growth_mb} MB",
                lane.sup_rss.growth() <= args.max_growth_mb,
                f"{lane.sup_rss.growth():.2f} MB",
            )
        )
        if IS_LINUX:
            checks.append(
                (
                    f"{tag} handle growth <= {args.max_handle_growth}",
                    lane.sup_fds.growth() <= args.max_handle_growth,
                    f"{lane.sup_fds.growth():.0f}",
                )
            )
        else:
            # Explicit SKIP accounting: a platform-unavailable check must
            # not masquerade as a PASS in the gate table.
            checks.append((f"{tag} handle growth", None, "SKIP: fd sampling is Linux-only"))

    checks.append(("aggregate killed == expected", total_killed == expected, f"{total_killed}/{expected}"))
    checks.append(("aggregate reaped == expected", total_reaped == expected, f"{total_reaped}/{expected}"))
    checks.append(
        (
            "aggregate ambush+escape+stillborn == reaped",
            total_classes == total_reaped,
            f"{total_classes}/{total_reaped}",
        )
    )
    if total_reaped:
        agg_pct = 100.0 * total_ambush / total_reaped
        checks.append(
            (f"aggregate ambush >= {min_ambush:.0f}%", agg_pct + 1e-9 >= min_ambush, f"{agg_pct:.1f}%")
        )

    rate = total_killed / duration_s

    print("", flush=True)
    for lane in lanes:
        report = lane.report or {}
        lat = lane.latencies_us
        reaped = int(report.get("total_reaped", 0))
        ambush = int(report.get("ambush_kills", 0))
        pct = 100.0 * ambush / reaped if reaped else 0.0
        print(
            f"Lane {lane.index}: sup={lane.sup.pid if lane.sup else '-'} exit={lane.sup_exit} "
            f"killed={lane.killed} reaped={reaped} | ambush={ambush} ({pct:.1f}%) "
            f"escape={report.get('escape_kills', 0)} stillborn={report.get('stillborn_kills', 0)} "
            f"final_gen={report.get('final_generation', 0)}",
            flush=True,
        )
        print(
            f"  dispatch us p50={percentile(lat, 50):.1f} p95={percentile(lat, 95):.1f} "
            f"p99={percentile(lat, 99):.1f} max={max(lat) if lat else 0.0:.1f} | "
            f"rss_peak={lane.sup_rss.peak:.2f} MB growth={lane.sup_rss.growth():.2f} MB | "
            f"handles first/peak/last={lane.sup_fds.first or 0:.0f}/"
            f"{lane.sup_fds.peak:.0f}/{lane.sup_fds.last or 0:.0f} | "
            f"stale={lane.stale} dup={lane.duplicates} retries={lane.retries}",
            flush=True,
        )

    # ok is bool or None; None = platform-skipped (never counts as a pass).
    failed = [name for name, ok, _ in checks if ok is False]
    skipped = [name for name, ok, _ in checks if ok is None]
    print(f"\n[RESULT] {total_killed} killed in {duration_s:.2f} s ({rate:.1f} kills/s)", flush=True)
    for name, ok, detail in checks:
        label = "PASS" if ok else ("SKIP" if ok is None else "FAIL")
        print(f"  {label}  {name}" + (f"  ({detail})" if detail else ""))
    if skipped:
        print(f"  [skip accounting] {len(skipped)} check(s) skipped: platform-gated", flush=True)
    verdict = "PASS" if not failed else "FAIL"
    print(f"[VERDICT] {verdict}", flush=True)

    stats = {
        "verdict": verdict,
        "platform": sys.platform,
        "armed_hold": args.armed_hold,
        "min_ambush_pct": min_ambush,
        "max_growth_mb": args.max_growth_mb,
        "max_handle_growth": args.max_handle_growth if IS_LINUX else None,
        "lines": [
            {
                "lane": lane.index,
                "kills": lane.killed,
                "reaped": int((lane.report or {}).get("total_reaped", 0)),
                "true_ambush": int((lane.report or {}).get("ambush_kills", 0)),
                "clean_escape": int((lane.report or {}).get("escape_kills", 0)),
                "stillborn": int((lane.report or {}).get("stillborn_kills", 0)),
                "ambush_pct": (
                    100.0
                    * int((lane.report or {}).get("ambush_kills", 0))
                    / max(int((lane.report or {}).get("total_reaped", 0)), 1)
                ),
                "dispatch_us": {
                    "p50": percentile(lane.latencies_us, 50),
                    "p95": percentile(lane.latencies_us, 95),
                    "p99": percentile(lane.latencies_us, 99),
                    "max": max(lane.latencies_us) if lane.latencies_us else 0.0,
                },
                "resources": {
                    "rss_mb": lane.sup_rss.as_dict(),
                    "handles": lane.sup_fds.as_dict() if IS_LINUX else None,
                    "worker_rss_mb_peak": round(lane.worker_rss.peak, 3),
                },
                "stale": lane.stale,
                "duplicates": lane.duplicates,
                "retries": lane.retries,
                "error": lane.error,
                "supervisor_exit": lane.sup_exit,
                "report": lane.report,
            }
            for lane in lanes
        ],
        "totals": {
            "killed": total_killed,
            "reaped": total_reaped,
            "true_ambush": total_ambush,
            "clean_escape": total_escape,
            "stillborn": total_stillborn,
            "elapsed_s": duration_s,
            "kills_per_s": rate,
        },
        "checks": [{"name": name, "ok": ok, "detail": detail} for name, ok, detail in checks],
    }
    stats_path = resolve(repo, args.stats)
    stats_path.parent.mkdir(parents=True, exist_ok=True)
    stats_path.write_text(json.dumps(stats, indent=2))
    report_path = resolve(repo, args.report)
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(stats, indent=2))
    print(f"Stats JSON: {stats_path}\nReport JSON: {report_path}", flush=True)
    return 0 if verdict == "PASS" else 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lines", type=int, default=4)
    parser.add_argument("--kills-per-line", type=int, default=2500)
    parser.add_argument("--poll-ms", type=float, default=1.0)
    parser.add_argument("--stall-sec", type=float, default=20.0)
    parser.add_argument("--supervisor-wait", type=float, default=60.0)
    parser.add_argument("--max-growth-mb", type=float, default=2.0)
    parser.add_argument("--max-handle-growth", type=int, default=16)
    parser.add_argument("--armed-hold", action="store_true")
    parser.add_argument("--min-ambush-pct", type=float, default=-1.0)
    parser.add_argument("--aggressive", action="store_true")
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--profile", choices=["release", "debug"], default="release")
    parser.add_argument("--target", default="process_termination_loop")
    parser.add_argument("--lane-root", default="target/multi_lineage")
    parser.add_argument("--report", default="target/multi_lineage_report.json")
    parser.add_argument("--stats", default="target/multi_lineage_stats.json")
    args = parser.parse_args()
    if not 1 <= args.lines <= 32:
        parser.error("--lines must be in 1..32")
    if args.kills_per_line <= 0:
        parser.error("--kills-per-line must be > 0")
    return args


def main() -> int:
    args = parse_args()
    repo = Path(__file__).resolve().parent.parent
    # SIGTERM (CI job cancellation) must unwind into run()'s finally so
    # the storm leaves no spinning supervisors or workers behind.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    return run(args, repo)


if __name__ == "__main__":
    sys.exit(main())
