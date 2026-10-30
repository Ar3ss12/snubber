#!/usr/bin/env python3
"""Name the Python defect a syntax check cannot see: a local read but never bound.

The POSIX storm failed twice in CI with
``NameError: name 'rate_windows' is not defined`` raised at the monitor thread's
construction — after the supervisors were spawned, so the run left live lanes
and no report. ``py_compile`` stayed green and both flavors died on it. A name
used and never assigned is a runtime defect, and the AST is where it is cheap
to find.

Run it over the scripts a job executes (a directory means every ``*.py`` in it):
    python3 scripts/check_python_bindings.py scripts

Prove the gate itself still bites before trusting a green run:
    python3 scripts/check_python_bindings.py --self-test

Exit code is 1 on any finding, so it gates a workflow step.
"""

from __future__ import annotations

import ast
import builtins
import os
import sys

BUILTIN_NAMES = set(dir(builtins)) | {
    "__file__",
    "__name__",
    "__doc__",
    "__builtins__",
    "__spec__",
    "__package__",
}
FUNCTION_TYPES = (ast.FunctionDef, ast.AsyncFunctionDef)
SCOPE_TYPES = (*FUNCTION_TYPES, ast.Lambda, ast.ClassDef)

# (label, source, names that must be reported). The names are asserted, not the
# wording and not a bare count, so neither a reworded report nor a finding
# raised for the wrong symbol can quietly disarm a case.
SELF_TEST = [
    ("local read, never bound", "def run(monitor, args):\n    return monitor(args, rate_windows)\n", ("rate_windows",)),
    ("unbound read inside a lambda", "def rollup(rows):\n    return sorted(rows, key=lambda kv: kv[1] * factor)\n", ("factor",)),
    ("module level read, never bound", "print(marker)\n", ("marker",)),
    ("lambda parameter", "def rollup(rows):\n    return sorted(rows, key=lambda kv: kv[1])\n", ()),
    ("comprehension target", "def biggest(recs, top_n):\n    return [r for r in recs if r['zone']][:top_n]\n", ()),
    ("except alias", "def load(path):\n    try:\n        return open(path).read()\n    except OSError as exc:\n        return str(exc)\n", ()),
    (
        "for, with and walrus targets",
        "def walk(items):\n    total = 0\n    with open('f') as handle:\n"
        "        for n in items:\n            if (step := int(n)):\n                total += step\n    return total\n",
        (),
    ),
    ("closure reads an enclosing local", "def outer():\n    limit = 5\n    def inner(n):\n        return n < limit\n    return inner\n", ()),
    ("module name read in a function", "FACTOR = 2\n\n\ndef twice(n):\n    return n * FACTOR\n", ()),
    ("default value evaluated in the enclosing scope", "def outer():\n    step = 1\n    def inner(n=step):\n        return n + 1\n    return inner()\n", ()),
    ("decorator evaluated in the enclosing scope", "def outer():\n    hook = repr\n    @hook\n    def inner():\n        return 1\n    return inner\n", ()),
    # A method reading a class constant is a real NameError, and this gate does
    # NOT report it: class-scope names stay visible downward so a decorated or
    # constant-heavy class never false-fires. Deliberate blind spot, pinned.
    ("class constant read in a method", "class Board:\n    LIMIT = 4\n\n    def room(self):\n        return LIMIT\n", ()),
]


def _arg_names(args: ast.arguments, names: set[str]) -> None:
    for group in (args.posonlyargs, args.args, args.kwonlyargs):
        names.update(arg.arg for arg in group)
    for vararg in (args.vararg, args.kwarg):
        if vararg is not None:
            names.add(vararg.arg)


def _collect(node: ast.AST, names: set[str]) -> None:
    if isinstance(node, SCOPE_TYPES):
        # A nested scope binds no name here except its own, and its body is
        # that scope's business.
        if not isinstance(node, ast.Lambda):
            names.add(node.name)
        return
    if isinstance(node, ast.Name) and isinstance(node.ctx, (ast.Store, ast.Del)):
        names.add(node.id)
    elif isinstance(node, ast.ExceptHandler) and node.name:
        names.add(node.name)
    elif isinstance(node, ast.NamedExpr) and isinstance(node.target, ast.Name):
        names.add(node.target.id)
    elif isinstance(node, (ast.Global, ast.Nonlocal)):
        names.update(node.names)
    elif isinstance(node, (ast.Import, ast.ImportFrom)):
        names.update(alias.asname or alias.name.split(".")[0] for alias in node.names)
    for child in ast.iter_child_nodes(node):
        _collect(child, names)


def own_bindings(node: ast.AST, names: set[str]) -> None:
    """Names this scope body binds, without descending into a nested scope."""
    if isinstance(node, (*FUNCTION_TYPES, ast.Lambda)):
        _arg_names(node.args, names)
    children = node.body if isinstance(node, (ast.Module, ast.ClassDef)) else ast.iter_child_nodes(node)
    for child in children:
        _collect(child, names)


def parent_side(scope: ast.AST) -> list[ast.expr]:
    """Sub-expressions of a scope that Python evaluates in the scope ENCLOSING it."""
    if isinstance(scope, (*FUNCTION_TYPES, ast.Lambda)):
        args = scope.args
        out = list(args.defaults) + [d for d in args.kw_defaults if d is not None]
        out += [a.annotation for a in (*args.posonlyargs, *args.args, *args.kwonlyargs) if a.annotation]
        for vararg in (args.vararg, args.kwarg):
            if vararg is not None and vararg.annotation:
                out.append(vararg.annotation)
        if not isinstance(scope, ast.Lambda):
            out += list(scope.decorator_list)
            if scope.returns:
                out.append(scope.returns)
        return out
    if isinstance(scope, ast.ClassDef):
        return list(scope.decorator_list) + list(scope.bases) + [k.value for k in scope.keywords]
    return []


def load_names(scope: ast.AST) -> list[ast.Name]:
    """Load references resolved in this scope's own namespace."""
    skip = {id(expr) for root in parent_side(scope) for expr in ast.walk(root)}
    found: list[ast.Name] = []

    def descend(node: ast.AST, owned: bool) -> None:
        if owned and isinstance(node, SCOPE_TYPES):
            for root in parent_side(node):
                descend(root, False)
            return
        if id(node) in skip:
            return
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load):
            found.append(node)
        for child in ast.iter_child_nodes(node):
            descend(child, True)

    descend(scope, False)
    return found


def collect_scopes(node: ast.AST, visible: set[str], owner: str, out: list[tuple]) -> None:
    for child in ast.iter_child_nodes(node):
        if isinstance(child, SCOPE_TYPES):
            own: set[str] = set()
            own_bindings(child, own)
            inner = own | visible
            label = owner + " lambda" if isinstance(child, ast.Lambda) else child.name
            out.append((label, child, inner))
            collect_scopes(child, inner, label, out)
        else:
            collect_scopes(child, visible, owner, out)


def unbound_refs(source: str) -> list[tuple[str, str, int]]:
    """(scope label, name, line) for every read that no scope binds."""
    tree = ast.parse(source, filename="<source>")
    module_names: set[str] = set()
    own_bindings(tree, module_names)
    scopes: list[tuple] = [("<module>", tree, module_names)]
    collect_scopes(tree, module_names, "<module>", scopes)

    refs: list[tuple[str, str, int]] = []
    seen: set[tuple[int, str]] = set()
    for label, node, visible in scopes:
        for ref in load_names(node):
            if ref.id in visible or ref.id in BUILTIN_NAMES:
                continue
            if (ref.lineno, ref.id) in seen:
                continue
            seen.add((ref.lineno, ref.id))
            refs.append((label, ref.id, ref.lineno))
    return refs


def find_unbound(source: str, path: str) -> list[str]:
    findings = []
    for label, name, lineno in unbound_refs(source):
        where = "module level" if label == "<module>" else f"{label}()"
        findings.append(f"{path}:{lineno}: {where} reads `{name}`, which nothing binds")
    return findings


def check(path: str) -> list[str]:
    with open(path, encoding="utf-8") as handle:
        return find_unbound(handle.read(), path)


def self_test() -> int:
    wrong = []
    for label, source, expected in SELF_TEST:
        got = tuple(sorted({name for _, name, _ in unbound_refs(source)}))
        if got != tuple(sorted(expected)):
            wrong.append(f"{label}: expected {sorted(expected)}, got {list(got)}")
    for line in wrong:
        print(f"SELF-TEST FAIL: {line}")
    print(f"SELF-TEST: {len(SELF_TEST)} cases, {len(wrong)} wrong")
    return 1 if wrong else 0


def expand_targets(argv: list[str]) -> list[str]:
    """Every *.py in a directory, so one CI step means the same on POSIX and PowerShell.

    PowerShell does not expand an unquoted glob, so `scripts/*.py` reaches the
    process as one literal path there and the gate would check nothing.
    """
    targets: list[str] = []
    for path in argv:
        if os.path.isdir(path):
            targets += sorted(
                os.path.join(path, name)
                for name in os.listdir(path)
                if name.endswith(".py")
            )
        elif path.endswith(".py"):
            targets.append(path)
    return targets


def main(argv: list[str]) -> int:
    if "--self-test" in argv:
        return self_test()
    targets = expand_targets(argv)
    if not targets:
        print("usage: check_python_bindings.py [--self-test] FILE_OR_DIRECTORY...", file=sys.stderr)
        return 1
    findings: list[str] = []
    for path in targets:
        try:
            findings += check(path)
        except (OSError, ValueError, SyntaxError) as exc:
            findings.append(f"{path}: unreadable: {type(exc).__name__}: {exc}")
    for line in findings:
        print(line)
    print(f"UNBOUND: {len(findings)} across {len(targets)} files")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
