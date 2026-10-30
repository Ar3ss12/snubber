#!/usr/bin/env python3
"""Publish storm evidence where a reader can reach it without signing in.

A failed storm step normally leaves its whole diagnosis in the step log, and
the step log of a public repository is only readable by someone signed in.
That made the first POSIX storm failure unanswerable: the job was 39 s old, the
step had run for under a second, and the artifact was 1.4 KB — which is only
interpretable as "the harness died early", never as *where*.

This script turns a storm's two artifacts into the channels that are public:

  * ``::error::`` / ``::warning::`` / ``::notice::`` workflow commands, which
    GitHub lists as annotations on the check run and returns from the
    anonymous ``/check-runs/<id>/annotations`` API;
  * Markdown in ``$GITHUB_STEP_SUMMARY``, which renders on the run page.

It reports and never decides: the exit code is always 0, so a broken summary
can never mask the storm's own verdict. And because a storm that died before
writing a report has nothing to annotate, the transcript tail is read too —
the last lines and any traceback are the finding.

Usage:
    python3 scripts/ci_storm_summary.py LABEL REPORT.json TRANSCRIPT.log [LABEL REPORT.log ...]
"""

from __future__ import annotations

import json
import os
import sys

# Console lines that identify what ran and on what. They are the first thing a
# reader needs when the run died later, and the harness prints them before it
# spawns anything.
HEADER_MARKS = ("[CONFIG]", "[SYSTEM]", "[TOOLCHAIN]", "[BUILD]", "[VERDICT]")
TAIL_LINES = 25


def announce(line: str, level: str) -> None:
    # A workflow command must be one line; %0A is the escape GitHub renders
    # back as a newline inside the annotation body.
    print(f"::{level}::{line.replace(chr(10), '%0A')}", flush=True)


def read_text(path: str) -> str | None:
    try:
        with open(path, encoding="utf-8-sig", errors="replace") as handle:
            return handle.read()
    except OSError:
        return None


def markdown(rows: list[str]) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write("\n".join(rows) + "\n")
    except OSError:
        pass


def headline(payload: dict) -> str:
    totals = {
        "verdict": payload.get("verdict"),
        "report_schema": payload.get("report_schema"),
        "lines": payload.get("lines"),
        "kills_completed": payload.get("kills_completed"),
        "true_ambush": payload.get("true_ambush"),
        "clean_escape": payload.get("clean_escape"),
        "stillborn": payload.get("stillborn"),
        "tokens_rescued": payload.get("tokens_rescued"),
        "clean_settles": payload.get("clean_settles"),
        "ambush_pct": payload.get("ambush_pct"),
        "duration_ms": payload.get("duration_ms"),
        "rate_per_sec": payload.get("rate_per_sec"),
    }
    return ", ".join(f"{k}={v}" for k, v in totals.items() if v is not None)


def transcript_rows(transcript: str | None) -> tuple[list[str], list[str]]:
    """Machine identity plus the tail, in a form safe to put in an annotation."""
    if transcript is None:
        return [], []
    lines = [line.rstrip() for line in transcript.splitlines()]
    headers = [line for line in lines if line.startswith(HEADER_MARKS)]
    tail = [line for line in lines if line.strip()][-TAIL_LINES:]
    rows = []
    if headers:
        rows += ["```"] + headers[:12] + ["```"]
    if tail:
        rows += ["", "Last transcript lines:", "", "```"] + tail + ["```"]
    return rows, tail


def report_rows(label: str, payload: dict, checks: list[dict]) -> tuple[list[str], list[str]]:
    failed = [c for c in checks if c.get("ok") is False]
    skipped = [c for c in checks if c.get("ok") is None]
    rows = ["", f"`{headline(payload)}`", ""]
    if failed:
        rows += ["| failed gate | detail |", "|---|---|"]
        rows += [f"| `{c.get('name')}` | {c.get('detail') or '-'} |" for c in failed]
    if skipped:
        rows += ["", "| skipped gate | why |", "|---|---|"]
        rows += [f"| `{c.get('name')}` | {c.get('detail') or '-'} |" for c in skipped]
    rows += ["", f"{len(checks)} gates, {len(failed)} red, {len(skipped)} skipped."]

    notes = [
        f"{label}: gate `{c.get('name')}` failed - {c.get('detail') or 'no detail'}"
        for c in failed
    ]
    if skipped:
        notes.append(
            f"{label}: {len(skipped)} gate(s) skipped - "
            + "; ".join(f"{c.get('name')} ({c.get('detail') or '-'})" for c in skipped)
        )
    return rows, notes


def report_missing(label: str, report_path: str, tail: list[str]) -> list[str]:
    if not tail:
        return [f"{label}: no storm report at {report_path} and no transcript tail to read"]
    return [f"{label}: exited before writing {report_path}. Transcript tail:\n" + "\n".join(tail[-6:])]


def one_storm(label: str, report_path: str, log_path: str) -> list[str]:
    transcript = read_text(log_path)
    body_rows, tail = transcript_rows(transcript)

    raw = read_text(report_path)
    if raw is None:
        if transcript is None:
            # Neither artifact exists: this storm never ran (the job aborted in
            # an earlier step). Saying "exited before writing a report" here
            # would blame a run that was never started.
            announce(f"{label}: not attempted - no report and no transcript at {log_path}", "warning")
            print(f"[SUMMARY] {label}: not attempted", flush=True)
            return ["", f"### {label}: not attempted"]
        for note in report_missing(label, report_path, tail):
            announce(note, "error")
        print(f"[SUMMARY] {label}: no report; transcript tail follows", flush=True)
        for line in tail[-10:]:
            print(f"    {line}", flush=True)
        return ["", f"### {label}: no report written", *body_rows]

    try:
        payload = json.loads(raw)
    except ValueError as exc:
        announce(f"{label}: report {report_path} unreadable: {type(exc).__name__}: {exc}", "error")
        return ["", f"### {label}: report unreadable ({type(exc).__name__})", *body_rows]

    checks = payload.get("checks") or []
    verdict = str(payload.get("verdict", "MISSING"))
    rows, notes = report_rows(label, payload, checks)
    for note in notes:
        announce(note, "error" if verdict == "FAIL" else "warning")
    if verdict == "PASS":
        announce(f"{label}: PASS - {headline(payload)}", "notice")
    print(f"[SUMMARY] {label}: {verdict} | {len(checks)} gates | {len(notes)} gate notes", flush=True)
    return ["", f"### {label}: `{verdict}`", *rows, *body_rows]


def main(argv: list[str]) -> int:
    if len(argv) % 3 != 0:
        print("usage: ci_storm_summary.py LABEL REPORT.json TRANSCRIPT.log [...]", file=sys.stderr)
        return 0

    rows = ["## Storm gates", ""]
    for index in range(0, len(argv), 3):
        label, report_path, log_path = argv[index : index + 3]
        rows += one_storm(label, report_path, log_path)

    markdown(rows[: 400 + 100 * (len(argv) // 3)])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
