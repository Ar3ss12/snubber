#!/usr/bin/env python3
"""snubber codebase analyzer (internal, for us only).

Usage:
  python scripts/analyze_codebase.py [--root .] [--out CODEBASE_REPORT.md]

What it measures (stdlib only, Windows-safe):
  lines/code/comments/doc per file+ext, Rust architecture (pub API, fns,
  structs/enums/traits, unsafe/unwrap/expect/saturating/locks/atomics,
  track_caller/must_use/test/bench sites), comment quality, TODO markers,
  per-dir breakdown, biggest files, git tail, Cargo version/features.
Binary blobs (.gif/.exe/...) counted by size only, never line-scanned.
Own report excluded so reruns do not inflate themselves.
"""

import argparse, json, re, subprocess
from pathlib import Path
from collections import Counter, defaultdict
from datetime import datetime, timezone

SKIP = {"target", ".git", "__pycache__", ".venv", "venv", "node_modules"}
# Binary extensions: counted by size only, never line-scanned (a .gif is
# not 70k lines of code — it is one blob).
BINARY_EXT = {".gif", ".png", ".jpg", ".jpeg", ".ico", ".exe", ".dll", ".pdb",
              ".zip", ".gz", ".bin", ".dat", ".woff", ".woff2", ".ttf"}
# Self-report + scratch: the analyzer must not count its own output.
SKIP_FILES = {"CODEBASE_REPORT.md"}
# NOTE: MARK intentionally lists TODO etc. to FIND them — the two hits
# inside this script are the pattern itself, not real debt.
MARK = re.compile(r"\b(TODO|FIXME|XXX|HACK|KLUDGE|BUG|UNWRAP|panic!)\b")
RUST_PAT = {
 "pub_fn": r"^\s*pub\s+(?:unsafe\s+)?fn\s+(\w+)",
 "pub_struct": r"^\s*pub\s+struct\s+(\w+)",
 "pub_enum": r"^\s*pub\s+enum\s+(\w+)",
 "pub_trait": r"^\s*pub\s+trait\s+(\w+)",
 "fn": r"^\s*(?:pub(?:\([^)]*\))?\s+)?fn\s+(\w+)",
}
RUST_CNT = ["unsafe", "unwrap", "expect", "saturating_", "lock()", "Atomic",
            "track_caller", "must_use", "test]", "ignore", "bench_function"]

def analyze_file(p: Path):
    try:
        txt = p.read_text(encoding="utf-8", errors="replace")
    except Exception as e:
        return {"err": str(e)}
    lines = txt.splitlines()
    n, blank, comment, doc, code = len(lines), 0, 0, 0, 0
    todos = []
    for i, ln in enumerate(lines, 1):
        s = ln.strip()
        if not s:
            blank += 1
            continue
        if s.startswith("///") or s.startswith("//!") or s.startswith("//"):
            comment += 1
            if s.startswith("///") or s.startswith("//!"):
                doc += 1
        elif p.suffix in (".ps1", ".sh", ".py", ".toml", ".yml") and s.startswith("#"):
            comment += 1
        else:
            code += 1
        m = MARK.search(ln)
        if m:
            todos.append((i, m.group(1), ln.strip()[:110]))
    d = {"path": str(p), "ext": p.suffix or "<noext>", "lines": n,
         "blank": blank, "comment": comment, "doc": doc, "code": code,
         "todos": todos, "size": p.stat().st_size}
    if p.suffix == ".rs":
        for k, pat in RUST_PAT.items():
            d[k] = re.findall("(?m)" + pat, txt)
        for k in RUST_CNT:
            d[k] = txt.count(k)
        d["fn_count"] = len(re.findall(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?fn\s+\w+", txt))
        d["derives"] = re.findall(r"#\[derive\(([^]]+)\)\]", txt)
    if p.suffix == ".md":
        d["headings"] = len(re.findall(r"(?m)^#{1,6}\s", txt))
        d["tables"] = len(re.findall(r"(?m)^\|.*\|\s*$", txt))
        d["codeblocks"] = txt.count("```") // 2
        d["cyr"] = len(re.findall(r"[А-Яа-яЁёІіЇїЄєҐґ]", txt))
    return d
def build_report(root: Path, recs):
    tot = Counter()
    ext = Counter()
    lang = defaultdict(int)
    for r in recs:
        tot["files"] += 1
        tot["lines"] += r["lines"]
        tot["blank"] += r["blank"]
        tot["comment"] += r["comment"]
        tot["code"] += r["code"]
        tot["doc"] += r["doc"]
        tot["size"] += r["size"]
        ext[r["ext"]] += 1
        lang[r["ext"]] += r["lines"]
    rs = [r for r in recs if r["ext"] == ".rs"]
    L = []
    L.append(f"tot_files={tot['files']} tot_lines={tot['lines']} code={tot['code']} "
             f"comment={tot['comment']} doc={tot['doc']} blank={tot['blank']} kib={tot['size']/1024:.1f}")
    L.append("EXT:" + ",".join(f"{e}={c}/{lang[e]}ln" for e, c in ext.most_common()))
    fn_t = sum(r.get("fn_count", 0) for r in rs)
    pub_t = sum(len(r.get("pub_fn", [])) for r in rs)
    L.append(f"RUST: files={len(rs)} fns={fn_t} pub_fns={pub_t} "
             f"unsafe={sum(r.get('unsafe',0) for r in rs)} "
             f"unwrap={sum(r.get('unwrap',0) for r in rs)} "
             f"expect={sum(r.get('expect',0) for r in rs)} "
             f"saturating={sum(r.get('saturating_',0) for r in rs)} "
             f"locks={sum(r.get('lock()',0) for r in rs)} "
             f"atomics={sum(r.get('Atomic',0) for r in rs)}")
    L.append(f"TESTS: attrs={sum(r.get('test]',0) for r in recs)} "
             f"ignored={sum(r.get('ignore',0) for r in recs)} "
             f"benches={sum(r.get('bench_function',0) for r in recs)}")
    return tot, ext, L, rs

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=".")
    ap.add_argument("--out", default="CODEBASE_REPORT.md")
    args = ap.parse_args()
    root = Path(args.root).resolve()
    # Text corpus: skip build/cache dirs, binary blobs, and our own report
    # (counting the report would inflate every next run).
    files = [p for p in root.rglob("*") if p.is_file()
             and not any(s in p.parts for s in SKIP)
             and p.suffix.lower() not in BINARY_EXT
             and p.name not in SKIP_FILES]
    recs = [analyze_file(p) for p in files]
    recs = [r for r in recs if "err" not in r]
    tot, ext, L, rs = build_report(root, recs)
    big = sorted(recs, key=lambda r: r["lines"], reverse=True)[:15]
    todos = [(r["path"], i, k, t) for r in recs for (i, k, t) in r["todos"]]
    try:
        m = __import__("re").search(r'version\s*=\s*"([^"]+)"',
            (root / "Cargo.toml").read_text(encoding="utf-8", errors="replace"))
        ver = m.group(1) if m else "n/a"
    except Exception:
        ver = "n/a"
    try:
        git = subprocess.run(["git", "log", "--oneline", "-5"], cwd=root,
                             capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:
        git = "n/a"
    R = [f"# Codebase report — {root.name} v{ver}"]
    R.append(f"_{datetime.now(timezone.utc):%Y-%m-%d %H:%M UTC}_")
    R.append("")
    R.append(f"**Totals:** {tot['files']} files, {tot['lines']} lines "
             f"({tot['code']} code, {tot['comment']} comment incl. {tot['doc']} doc, "
             f"{tot['blank']} blank), {tot['size']/1024:.1f} KiB")
    R.append(f"comment_ratio={tot['comment']/max(1,tot['lines'])*100:.1f}% "
             f"doc_ratio={tot['doc']/max(1,tot['lines'])*100:.1f}%")
    R.append("\n## Extensions")
    for e, c in ext.most_common():
        lang_ln = sum(r["lines"] for r in recs if r["ext"] == e)
        R.append(f"- `{e}`: {c} files, {lang_ln} lines")
    R.append("\n## Rust summary")
    for ln in L[2:]:
        R.append(f"- {ln}")
    R.append("\n## Biggest 15 files")
    for r in big:
        rel = Path(r["path"]).relative_to(root)
        R.append(f"- `{rel}` — {r['lines']} ln ({r['code']} code, {r['doc']} doc)")
    R.append(f"\n## Markers: {len(todos)}")
    for p, i, k, t in todos[:40]:
        R.append(f"- {k} `{Path(p).relative_to(root)}:{i}` {t}")
    if len(todos) > 40:
        R.append(f"- ... +{len(todos)-40} more")
    R.append("\n## Recent git\n```\n" + git + "\n```")
    rep = "\n".join(R)
    print(rep)
    Path(args.out).write_text(rep, encoding="utf-8")
    print(f"\n[saved] {args.out}")

if __name__ == "__main__":
    main()
