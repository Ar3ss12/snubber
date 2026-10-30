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
(macOS), so a recycled pid is never mistaken for the live worker. Descriptor
growth is sampled from ``/proc/<pid>/fd`` on Linux and ``lsof -p`` on macOS.

Every reap is classified from the supervisor report: true ambush (armed token
at death), clean escape (commit seen) or stillborn (neither). ``--armed-hold``
parks the worker armed and publishes its pid only after the arm frame, so
``--min-ambush-pct 70`` proves armed workers were killed, not startup noise.

Three artifacts, each with one job, with the SAME key names and key order as
``stress_multi_lineage.ps1`` so tests/doc_sync.rs pins a POSIX run and a
Windows run through one oracle:
  ``--report`` headline totals, per-lane gates, checks, verdict, machine
             identity, dispatch histogram, top-10 slowest kills. This is the
             file docs/evidence commits.
  ``--stats``  distribution detail: per-lane raw dispatch series and bucket
             counts, the 250 ms rate trace, per-lane killer counters.
  ``--log``    full console transcript (stdout and stderr mirrored to a file).

Exit code 0 = PASS, 1 = FAIL.
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
import os
import platform
import signal
import shutil
import subprocess
import sys
import threading
import traceback
import time
from pathlib import Path

IS_LINUX = sys.platform.startswith("linux")

# Descriptor sampling: /proc on Linux, `lsof -p` on macOS. Probe once so a
# platform without either still reports an explicit SKIP instead of silently
# zero samples.
LSOF = shutil.which("lsof") or (
    "/usr/sbin/lsof" if Path("/usr/sbin/lsof").exists() else None
)
FD_SAMPLING = IS_LINUX or LSOF is not None

# Set by main() once the transcript is open; both JSON payloads report it, so
# a run that left no log is visible in the artifact rather than in a terminal
# someone already closed.
RUN_LOG: dict = {"path": None, "written": False}


def resolve(repo: Path, value: str) -> Path:
    p = Path(value)
    return p if p.is_absolute() else repo / p


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, math.ceil(pct / 100.0 * len(ordered)) - 1))
    return ordered[idx]


# Same buckets and same key names as scripts/stress_multi_lineage.ps1: one
# doc_sync oracle has to read a POSIX artifact and a Windows artifact without
# branching on which harness wrote them.
HIST_BUCKETS = [50, 100, 200, 500, 1000, 2000, 5000, 10000]


def histogram_us(values: list[float]) -> dict[str, int]:
    ordered = sorted(values)
    out: dict[str, int] = {}
    prev = 0
    for bucket in HIST_BUCKETS:
        up_to = sum(1 for v in ordered if v <= bucket)
        out[f"band_{bucket}"] = up_to - prev
        prev = up_to
    out["band_above_10000"] = len(ordered) - prev
    return out


def dispatch_block(values: list[float]) -> dict[str, float]:
    return {
        "min": round(percentile(values, 0.0), 1),
        "p50": round(percentile(values, 50), 1),
        "p95": round(percentile(values, 95), 1),
        "p99": round(percentile(values, 99), 1),
        "p999": round(percentile(values, 99.9), 1),
        "max": round(max(values) if values else 0.0, 1),
        "count": len(values),
    }


def top_slowest(lanes: list["Lane"], limit: int = 10) -> list[dict]:
    pairs = [(v, lane.index) for lane in lanes for v in lane.latencies_us]
    pairs.sort(reverse=True)
    return [{"lane": idx, "dispatch_us": round(v, 1)} for v, idx in pairs[:limit]]


def shell_out(cmd: list[str]) -> str | None:
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    text = out.stdout.strip()
    return text or None


def system_identity(profile: str) -> dict:
    """Machine the storm ran on.

    BENCHMARKS.md has always said "release build on an AMD Ryzen 5 4600H";
    until this block nothing in the artifact proved it, and a number quoted
    from a laptop could not be told apart from one quoted from a runner.
    """
    info = {
        "cpu_name": None,
        "physical_cores": None,
        "logical_cores": os.cpu_count(),
        "os_caption": f"{platform.system()} {platform.release()}",
        "os_build": platform.release(),
        "rustc": shell_out(["rustc", "--version"]),
        "python": sys.version.split()[0],
        "profile": profile,
    }
    if IS_LINUX:
        try:
            text = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
        except OSError:
            text = ""
        for line in text.splitlines():
            if line.startswith("model name"):
                info["cpu_name"] = line.split(":", 1)[1].strip()
                break
        cores = [
            int(line.split(":", 1)[1])
            for line in text.splitlines()
            if line.startswith("cpu cores")
        ]
        sockets = len({
            line.split(":", 1)[1].strip()
            for line in text.splitlines()
            if line.startswith("physical id")
        })
        if cores and sockets:
            info["physical_cores"] = cores[0] * sockets
        os_release = Path("/etc/os-release")
        if os_release.exists():
            for line in os_release.read_text(encoding="utf-8", errors="replace").splitlines():
                if line.startswith("PRETTY_NAME="):
                    info["os_caption"] = line.split("=", 1)[1].strip('"')
                    break
    else:
        brand = shell_out(["sysctl", "-n", "machdep.cpu.brand_string"]) or shell_out(
            ["sysctl", "-n", "hw.model"]
        )
        info["cpu_name"] = brand
        phys = shell_out(["sysctl", "-n", "hw.physicalcpu"])
        if phys and phys.isdigit():
            info["physical_cores"] = int(phys)
    return info


class Tee:
    """Mirror a stream into the run log — the Start-Transcript counterpart.

    A CI failure has to be readable without re-deriving it from JSON, and the
    two harnesses must leave the same kind of artifact on both platforms.
    """

    def __init__(self, stream, handle) -> None:
        self.stream = stream
        self.handle = handle

    def write(self, data: str) -> int:
        self.stream.write(data)
        try:
            self.handle.write(data)
            self.handle.flush()
        except (OSError, ValueError):
            pass
        return len(data)

    def flush(self) -> None:
        self.stream.flush()

    def isatty(self) -> bool:
        try:
            return self.stream.isatty()
        except (AttributeError, ValueError):
            return False


def lane_stderr(lane: "Lane", limit: int = 12) -> str:
    """Tail of one supervisor's own error stream.

    A supervisor that dies before publishing a pid leaves the reason in
    ``supervisor.err`` and nothing printed it: the harness reported an exit
    code, so the transcript said *that* a lane failed but never *why*.
    """
    try:
        text = (lane.dir / "supervisor.err").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return "no supervisor.err on disk"
    lines = [line.rstrip() for line in text.splitlines() if line.strip()]
    if not lines:
        return "supervisor.err is empty"
    return " | ".join(lines[-limit:])


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
    if IS_LINUX:
        try:
            return len(os.listdir(f"/proc/{pid}/fd"))
        except OSError:
            return None
    if LSOF is None:
        return None
    # -nP skip host/port lookups; -F f emits one "f<fd>" line per descriptor.
    try:
        out = subprocess.run(
            [LSOF, "-nP", "-p", str(pid), "-F", "f"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0 or not out.stdout:
        return None
    return sum(1 for line in out.stdout.splitlines() if line.startswith("f"))


class Sample:
    def __init__(self) -> None:
        self.first: float | None = None
        self.last: float | None = None
        self.peak: float = 0.0
        self.count: int = 0

    def note(self, value: float | None) -> None:
        # A live process never reads 0 bytes or 0 file descriptors, so None and
        # <= 0 are both failed reads. They are dropped instead of recorded: a
        # post-exit zero kept as the last sample would read as the dip that
        # disarms the growth gate.
        if value is None or value <= 0:
            return
        if self.first is None:
            self.first = value
        self.last = value
        self.peak = max(self.peak, value)
        self.count += 1

    def growth(self) -> float:
        if self.first is None or self.last is None:
            return 0.0
        # Signed: a window that lost memory or descriptors is information, and
        # the gate is one-sided (only a rise can trip it).
        return self.last - self.first


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
        self.misses = 0
        self.stalled = False
        self.error: str | None = None
        self.elapsed_ms = 0.0
        self.latencies_us: list[float] = []
        self.sup_poll_drops = 0
        self.last_killed: tuple[int, str] | None = None
        self.report: dict | None = None
        self.sup_rss = Sample()
        self.worker_rss = Sample()
        self.sup_fds = Sample()
        self.lock = threading.Lock()


def lane_record(lane: Lane) -> dict:
    """One lane in the report, with the .ps1 lane-record key names."""
    report = lane.report or {}
    reaped = int(report.get("total_reaped", 0))
    ambush = int(report.get("ambush_kills", 0))
    return {
        "index": lane.index,
        "supervisor_pid": lane.sup.pid if lane.sup else 0,
        "supervisor_exit": lane.sup_exit,
        "killed": lane.killed,
        "stale": lane.stale,
        "duplicates": lane.duplicates,
        "retries": lane.retries,
        "miss_polls": lane.misses,
        "stalled": bool(lane.stalled),
        "error": lane.error,
        "elapsed_ms": lane.elapsed_ms,
        "dispatch_us": dispatch_block(lane.latencies_us),
        "classification": {
            "true_ambush": ambush,
            "escape": int(report.get("escape_kills", 0)),
            "stillborn": int(report.get("stillborn_kills", 0)),
            "ambush_pct": round(100.0 * ambush / reaped if reaped else 0.0, 1),
        },
        "report": lane.report,
        "resources": {
            "rss_peak_mb": round(lane.sup_rss.peak, 3),
            "rss_growth_mb": round(lane.sup_rss.growth(), 3),
            "handles_first": int(lane.sup_fds.first or 0),
            "handles_peak": int(lane.sup_fds.peak),
            "handles_last": int(lane.sup_fds.last or 0),
            "handles_growth": int(lane.sup_fds.growth()),
            "worker_rss_peak_mb": round(lane.worker_rss.peak, 3),
            "samples": lane.sup_rss.count,
            "sample_drops": lane.sup_poll_drops,
            "fd_sampling": FD_SAMPLING,
        },
    }


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
    lane_started = time.monotonic()
    identity_cache: dict[int, tuple[str, str] | None] = {}

    try:
        while not stop.is_set() and lane.killed < args.kills_per_line:
            if lane.sup is not None and lane.sup.poll() is not None:
                lane.error = f"supervisor exited before {args.kills_per_line} kills"
                return

            published = read_pid(pid_file)
            if published is None:
                # Nothing readable this poll: the supervisor had not published
                # the replacement worker yet. miss_polls is the counter that
                # says the pid-file handshake, not the kill, set the pace.
                lane.misses += 1
                pid = None
            elif published in supervisors or published == os.getpid():
                pid = None
            else:
                pid = published

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
    finally:
        lane.elapsed_ms = round((time.monotonic() - lane_started) * 1000.0, 1)

    if lane.killed < args.kills_per_line and not stop.is_set():
        lane.stalled = True
        lane.error = "killer exited before reaching kills-per-line"


def monitor(
    args: argparse.Namespace,
    lanes: list[Lane],
    supervisors: set[int],
    stop: threading.Event,
    rate_windows: list[float],
) -> None:
    started = time.monotonic()
    last_line = 0.0
    last_sample = 0.0
    last_fd_sample = 0.0
    # Same 250 ms window as the .ps1 harness: an average over the whole storm
    # hides the burst that actually stresses the drain, so the peak is only
    # reproducible if the window itself is recorded.
    last_window = started
    window_seen = 0
    # lsof enumerates every descriptor; at 10 Hz per lane it would cost more
    # than the storm it measures. One sample per second still spans the run.
    fd_interval = 0.1 if IS_LINUX else 1.0
    while not stop.is_set():
        now = time.monotonic()
        if now - last_window >= 0.25:
            total_now = sum(lane.killed for lane in lanes)
            span = max(now - last_window, 0.001)
            rate_windows.append(round((total_now - window_seen) / span, 1))
            window_seen = total_now
            last_window = now
        # RSS reads spawn `ps` on macOS; sample resources at 10 Hz, print at 2 Hz.
        fd_due = now - last_fd_sample >= fd_interval
        if fd_due:
            last_fd_sample = now
        if now - last_sample >= 0.1:
            last_sample = now
            for lane in lanes:
                if lane.sup and lane.sup.poll() is None:
                    rss = proc_rss_mb(lane.sup.pid)
                    fds = proc_fds(lane.sup.pid) if fd_due else None
                    # One poll, one verdict: a poll of a live supervisor that
                    # yields no readable metric is counted as dropped, so the
                    # resource window keeps only real samples.
                    if rss is None or rss <= 0 or (fd_due and (fds is None or fds <= 0)):
                        lane.sup_poll_drops += 1
                    lane.sup_rss.note(rss)
                    if fd_due:
                        lane.sup_fds.note(fds)
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
    except ProcessLookupError:
        pass
    except OSError as exc:
        print(f"[WARN] lane {lane.index} kill(pid={worker}) failed: {exc}", file=sys.stderr)


def cleanup(lanes: list[Lane], supervisors: set[int], target: str) -> None:
    """Abort path: never leave spinning supervisors or workers behind."""
    for lane in lanes:
        if lane.sup is not None and lane.sup.poll() is None:
            print(f"[CLEANUP] lane {lane.index} killing supervisor pid={lane.sup.pid}", flush=True)
            lane.sup.kill()
            try:
                lane.sup.wait(timeout=5)
            except subprocess.TimeoutExpired:
                print(f"[WARN] lane {lane.index} supervisor {lane.sup.pid} ignored SIGKILL for 5 s", file=sys.stderr)
    for lane in lanes:
        kill_lane_worker(lane, supervisors, target)


def run(args: argparse.Namespace, repo: Path) -> int:
    if sys.platform == "win32":
        print("Windows uses scripts/stress_multi_lineage.ps1 (raw Win32 kill path).", file=sys.stderr)
        return 1

    exe = repo / "target" / args.profile / "examples" / args.target
    if args.build or not exe.exists():
        cmd = ["cargo", "build", "--locked", "--example", args.target]
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

    system = system_identity(args.profile)
    print(
        f"[CONFIG] lines={args.lines} kills_per_line={args.kills_per_line} "
        f"armed_hold={args.armed_hold} aggressive={args.aggressive} "
        f"profile={args.profile} platform={sys.platform}",
        flush=True,
    )
    print(
        f"[SYSTEM] {system['cpu_name'] or 'unknown cpu'} "
        f"({system['physical_cores']} physical / {system['logical_cores']} logical cores), "
        f"{system['os_caption']} build {system['os_build']}",
        flush=True,
    )
    print(
        f"[TOOLCHAIN] rustc: {system['rustc'] or 'not on PATH'} | "
        f"python {system['python']} | fd_sampling={FD_SAMPLING}",
        flush=True,
    )

    supervisors: set[int] = set()
    duration_s = 0.0
    finalized = False
    print(f"[LAUNCH] {args.lines} supervisors from {exe} (cwd per lane under {lane_root})", flush=True)
    try:
        for lane in lanes:
            try:
                spawn_supervisor(args, repo, lane)
            except OSError as exc:
                print(f"lane {lane.index} spawn failed: {exc}", file=sys.stderr)
                return 1
            supervisors.add(lane.sup.pid)

        deadline = time.monotonic() + 15
        print(f"[ARM] waiting for {args.lines} worker pid files (15 s budget)", flush=True)
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
                        f"(exit={lane.sup.returncode}): {lane_stderr(lane)}",
                        file=sys.stderr,
                    )
                    return 1
            time.sleep(0.02)
        else:
            reasons = {lane.index: lane_stderr(lane, 4) for lane in lanes if lane.sup}
            print(
                f"lanes published no worker pid within 15 s: {missing}; "
                f"last supervisor errors: {reasons}",
                file=sys.stderr,
            )
            return 1

        stop = threading.Event()
        killers = [
            threading.Thread(target=killer, args=(args, lane, supervisors, stop), daemon=True)
            for lane in lanes
        ]
        monitor_thread = threading.Thread(
            target=monitor, args=(args, lanes, supervisors, stop, rate_windows), daemon=True
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
    total_tokens_rescued = sum(int((lane.report or {}).get("tokens_rescued", 0)) for lane in lanes)
    total_clean_settles = sum(int((lane.report or {}).get("clean_settles", 0)) for lane in lanes)
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
        checks.append(
            (
                f"{tag} rescued+clean == reaped",
                int(report.get("tokens_rescued", 0)) + int(report.get("clean_settles", 0)) == reaped,
                f"{report.get('tokens_rescued', 0)}+{report.get('clean_settles', 0)}",
            )
        )
        checks.append((f"{tag} supervisor exit 0", lane.sup_exit == 0, str(lane.sup_exit)))
        checks.append(
            (f"{tag} killer no error/stall", lane.error is None and not lane.stalled, lane.error or "")
        )
        checks.append(
            (f"{tag} ambush >= {min_ambush:.0f}%", ambush_pct + 1e-9 >= min_ambush, f"{ambush_pct:.1f}%")
        )
        window_readable = lane.sup_rss.first is not None and lane.sup_rss.last is not None and (
            not FD_SAMPLING or (lane.sup_fds.first is not None and lane.sup_fds.last is not None)
        )
        checks.append(
            (
                f"{tag} resource window readable",
                window_readable,
                (
                    f"rss {lane.sup_rss.first}/{lane.sup_rss.last} MB over "
                    f"{lane.sup_rss.count} samples, {lane.sup_poll_drops} dropped polls"
                ),
            )
        )
        rss_ok = lane.sup_rss.count > 0 and lane.sup_rss.growth() <= args.max_growth_mb
        rss_detail = (
            f"{lane.sup_rss.growth():+.2f} MB"
            if lane.sup_rss.count > 0
            else "0 samples (leak gate cannot pass on no data)"
        )
        checks.append((f"{tag} rss growth <= {args.max_growth_mb} MB", rss_ok, rss_detail))
        if FD_SAMPLING:
            fds_ok = lane.sup_fds.count > 0 and lane.sup_fds.growth() <= args.max_handle_growth
            fds_detail = (
                f"{lane.sup_fds.growth():+.0f}"
                if lane.sup_fds.count > 0
                else "0 samples (leak gate cannot pass on no data)"
            )
            checks.append((f"{tag} handle growth <= {args.max_handle_growth}", fds_ok, fds_detail))
        else:
            # Explicit SKIP accounting: a platform-unavailable check must
            # not masquerade as a PASS in the gate table.
            checks.append((f"{tag} handle growth", None, "SKIP: no /proc and no lsof"))

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
            f"stale={lane.stale} dup={lane.duplicates} retries={lane.retries} "
            f"miss_polls={lane.misses} elapsed={lane.elapsed_ms:.0f} ms",
            flush=True,
        )

    # Distribution detail before the gate table: the buckets and the tail say
    # what the storm looked like; the checks below only say whether it cleared
    # its marks. Same lines the .ps1 prints.
    all_lat = [v for lane in lanes for v in lane.latencies_us]
    peak_rate = max(rate_windows) if rate_windows else 0.0
    killers_saw = {
        "stale": sum(lane.stale for lane in lanes),
        "duplicates": sum(lane.duplicates for lane in lanes),
        "retries": sum(lane.retries for lane in lanes),
        "miss_polls": sum(lane.misses for lane in lanes),
    }
    hist = histogram_us(all_lat)
    print(
        f"[DISPATCH] min={percentile(all_lat, 0.0):.1f} p50={percentile(all_lat, 50):.1f} "
        f"p95={percentile(all_lat, 95):.1f} p99={percentile(all_lat, 99):.1f} "
        f"p99.9={percentile(all_lat, 99.9):.1f} max={max(all_lat) if all_lat else 0.0:.1f} us",
        flush=True,
    )
    print(
        "           bands: "
        + " | ".join(
            f"{lo}-{b}: {hist[f'band_{b}']}"
            for lo, b in zip([0] + HIST_BUCKETS[:-1], HIST_BUCKETS)
        )
        + f" | >{HIST_BUCKETS[-1]}: {hist['band_above_10000']}"
        + f"  (disjoint band counts, sum {len(all_lat)})",
        flush=True,
    )
    print(
        f"[KILLERS] stale={killers_saw['stale']} duplicates={killers_saw['duplicates']} "
        f"retries={killers_saw['retries']} miss_polls={killers_saw['miss_polls']} "
        f"across {args.lines} lineages | peak {peak_rate:.1f} kills/s (250 ms window)",
        flush=True,
    )
    slowest = top_slowest(lanes)
    if slowest:
        print(
            "[TAIL] " + ", ".join(f"lane {s['lane']}={s['dispatch_us']:.1f} us" for s in slowest),
            flush=True,
        )
    print(
        "[SAMPLES] "
        + " | ".join(
            f"lane {lane.index}: rss {lane.sup_rss.count} samples "
            f"({lane.sup_rss.first}/{lane.sup_rss.last} MB), fd {lane.sup_fds.count} "
            f"({lane.sup_fds.first}/{lane.sup_fds.last}), {lane.sup_poll_drops} dropped polls"
            for lane in lanes
        ),
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
    # A skipped check is a hole, not a pass. In CI (GitHub Actions sets
    # CI=true/1) any skip fails the run; locally a platform-gated skip is
    # printed above and allowed (mirrors stress_harness.sh's in_ci rule).
    in_ci = os.environ.get("CI", "").lower() in ("true", "1")
    verdict = "PASS" if not failed and (not skipped or not in_ci) else "FAIL"
    print(f"[VERDICT] {verdict}", flush=True)

    report_doc = {
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "model": "multi_lineage",
        "report_schema": 2,
        "lines": args.lines,
        "kills_per_line": args.kills_per_line,
        "kills_requested": expected,
        "kills_completed": total_reaped,
        "workload": "spin" if args.aggressive else "sleep",
        "timer_resolution": None,
        "duration_ms": round(duration_s * 1000.0, 1),
        "rate_per_sec": round(rate, 1),
        "peak_rate_per_sec": round(peak_rate, 1),
        "tokens_rescued": total_tokens_rescued,
        "clean_settles": total_clean_settles,
        "armed_hold": bool(args.armed_hold),
        "min_ambush_pct": min_ambush,
        "true_ambush": total_ambush,
        "clean_escape": total_escape,
        "stillborn": total_stillborn,
        "ambush_pct": round(100.0 * total_ambush / total_reaped if total_reaped else 0.0, 1),
        "dispatch_us": dispatch_block(all_lat),
        "max_growth_mb": args.max_growth_mb,
        "max_handle_growth": args.max_handle_growth if FD_SAMPLING else None,
        "lanes": [lane_record(lane) for lane in lanes],
        "checks": [{"name": name, "ok": ok, "detail": detail} for name, ok, detail in checks],
        "verdict": verdict,
        # Everything below `verdict` is additive. tests/doc_sync.rs reads a key
        # by FIRST occurrence and the `dispatch_us` block by its first `p50`, so
        # a headline token must stay above this line or it gets shadowed. The
        # key order matches stress_multi_lineage.ps1 exactly, which is what lets
        # one oracle pin a POSIX artifact and a Windows artifact.
        "system": system,
        "killers_saw": killers_saw,
        "dispatch_histogram_us": hist,
        "slowest_dispatches": slowest,
        "transcript": {"path": RUN_LOG["path"], "written": RUN_LOG["written"]},
        # POSIX-only extras, after the shared set.
        "platform": sys.platform,
        "kills_dispatched": total_killed,
        "fd_sampling": FD_SAMPLING,
    }

    stats_doc = {
        "timestamp_utc": report_doc["timestamp_utc"],
        "model": "multi_lineage",
        "detail_of": args.report,
        "lines": args.lines,
        "kills_per_line": args.kills_per_line,
        "kills_completed": total_reaped,
        "verdict": verdict,
        "system": report_doc["system"],
        "dispatch_us": report_doc["dispatch_us"],
        "dispatch_histogram_us": report_doc["dispatch_histogram_us"],
        "slowest_dispatches": report_doc["slowest_dispatches"],
        "killers_saw": killers_saw,
        "rate_window_ms": 250,
        "rate_window_per_sec": rate_windows,
        "peak_rate_per_sec": round(peak_rate, 1),
        "lanes": [
            {
                "index": lane.index,
                "dispatch_us": dispatch_block(lane.latencies_us),
                "histogram_us": histogram_us(lane.latencies_us),
                "dispatch_us_series": [round(v, 1) for v in lane.latencies_us],
                "killers_saw": {
                    "stale": lane.stale,
                    "duplicates": lane.duplicates,
                    "retries": lane.retries,
                    "miss_polls": lane.misses,
                },
                "resources": {
                    "rss_peak_mb": round(lane.sup_rss.peak, 3),
                    "rss_growth_mb": round(lane.sup_rss.growth(), 3),
                    "handles_first": int(lane.sup_fds.first or 0),
                    "handles_peak": int(lane.sup_fds.peak),
                    "handles_last": int(lane.sup_fds.last or 0),
                    "handles_growth": int(lane.sup_fds.growth()),
                    "worker_rss_peak_mb": round(lane.worker_rss.peak, 3),
                    "samples": lane.sup_rss.count,
                    "sample_drops": lane.sup_poll_drops,
                    "fd_samples": lane.sup_fds.count,
                    "elapsed_ms": lane.elapsed_ms,
                },
            }
            for lane in lanes
        ],
        "transcript": {"path": RUN_LOG["path"], "written": RUN_LOG["written"]},
    }

    report_path = resolve(repo, args.report)
    stats_path = resolve(repo, args.stats)
    for path in (report_path, stats_path):
        path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report_doc, indent=2))
    stats_path.write_text(json.dumps(stats_doc, indent=2))
    print(f"Report JSON     : {report_path}", flush=True)
    print(f"Stats JSON      : {stats_path}", flush=True)
    if RUN_LOG["written"]:
        print(f"Transcript      : {resolve(repo, args.log)}", flush=True)
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
    parser.add_argument("--log", default="target/multi_lineage_run.log")
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
    RUN_LOG["path"] = args.log
    log_path = resolve(repo, args.log)
    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        handle = open(log_path, "w", encoding="utf-8")
    except OSError as exc:
        print(f"[WARN] run log {log_path} unavailable: {exc}", file=sys.stderr)
        return run(args, repo)
    out_stream, err_stream = sys.stdout, sys.stderr
    sys.stdout = Tee(out_stream, handle)
    sys.stderr = Tee(err_stream, handle)
    RUN_LOG["written"] = True
    try:
        return run(args, repo)
    except Exception as exc:  # noqa: BLE001 - an aborted storm must name itself
        # SystemExit (the SIGTERM cancellation path) is deliberately not
        # caught: it already unwinds through run()'s finally.
        # Exit code alone cannot be read on a runner without signing in; the
        # annotation puts the exception and where the transcript stopped on the
        # public check-run, next to the storm that produced it.
        traceback.print_exc()
        print(f"[ABORT] {type(exc).__name__}: {exc}", file=sys.stderr, flush=True)
        if os.environ.get("CI", "").lower() in ("true", "1"):
            print(
                f"::error::storm aborted with {type(exc).__name__}: {exc} - "
                f"see {args.log} (tail above the marker line)",
                flush=True,
            )
        return 1
    finally:
        sys.stdout, sys.stderr = out_stream, err_stream
        handle.close()


if __name__ == "__main__":
    sys.exit(main())
