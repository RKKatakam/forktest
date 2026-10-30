"""Interleaved A/B timing: plain pytest, the Python forktest and the Rust forktest on one project.

usage: python bench/bench_ab.py ROOT TESTPATH EDIT_FILE ANCHOR_LINE ROUNDS [--xdist] [--serial]

ROOT must hold a .venv with the project and pytest installed. The Python
forktest runs from the source tree in $FORKTEST_PY_SRC (a checkout of main),
the Rust one is $FORKTEST_BIN. Each round inserts a no-op statement above
ANCHOR_LINE, then times every side's edit run, every side's no-op rerun and
every side's --all, interleaved. A warm-up round fills both stores and is not
counted. Every forktest run's done counts are printed so parity is visible.
"""

import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

root, testpath, edit_file, anchor, rounds = sys.argv[1:6]
ROOT = Path(root)
PY = str(ROOT / ".venv/bin/python")
PY_SRC = os.environ["FORKTEST_PY_SRC"]
RS_BIN = os.environ["FORKTEST_BIN"]
TARGET = ROOT / edit_file
ORIGINAL = TARGET.read_text()
assert ORIGINAL.count(anchor) == 1, "anchor must appear exactly once"
indent = re.match(r"\s*", anchor).group(0)


def edited(variant: int) -> str:
    return ORIGINAL.replace(anchor, f"{indent}_forktest_bench_edit = {variant}\n{anchor}", 1)


def timed(cmd: list[str], env: dict[str, str]) -> tuple[float, str]:
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, env={**os.environ, **env})
    dt = time.perf_counter() - t0
    summary = f"exit {proc.returncode}"
    for line in proc.stdout.splitlines():
        if line.startswith('{"event":"done"'):
            d = json.loads(line)
            summary = " ".join(f"{k}={d[k]}" for k in ("passed", "failed", "errors", "skipped", "xfailed") if d.get(k))
        elif line.startswith('{"event":"selection"'):
            d = json.loads(line)
            summary = f"selected {d['selected']}/{d['total']} · "
        elif re.search(r"\d+ (passed|failed)", line):
            summary = line.strip("= ")
    if proc.returncode not in (0, 1):
        sys.stderr.write(proc.stderr[-2000:])
    return dt, summary


def python_forktest(extra: list[str]) -> list[str]:
    return [PY, "-m", "forktest", "--no-daemon", "--json", *extra, testpath]


def rust_forktest(extra: list[str]) -> list[str]:
    return [RS_BIN, "--no-daemon", "--json", *extra, testpath]


PY_ENV = {"PYTHONPATH": PY_SRC}
RS_ENV = {"FORKTEST_PYTHON": PY}
sides: dict[str, tuple[list[str], dict[str, str]]] = {
    "py edit": (python_forktest([]), PY_ENV),
    "rs edit": (rust_forktest([]), RS_ENV),
    "py no-op": (python_forktest([]), PY_ENV),
    "rs no-op": (rust_forktest([]), RS_ENV),
    "py --all": (python_forktest(["--all"]), PY_ENV),
    "rs --all": (rust_forktest(["--all"]), RS_ENV),
}
if "--xdist" in sys.argv:
    sides["pytest -n auto"] = ([PY, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-n", "auto", testpath], {})
if "--serial" in sys.argv:
    sides["pytest"] = ([PY, "-m", "pytest", "-q", "-p", "no:cacheprovider", testpath], {})

times: dict[str, list[float]] = {s: [] for s in sides}
try:
    for i in range(int(rounds) + 1):
        TARGET.write_text(edited(i % 2))
        print(f"round {i}{' (warm-up)' if i == 0 else ''}", flush=True)
        for side, (cmd, env) in sides.items():
            dt, summary = timed(cmd, env)
            print(f"  {side:<15} {dt:6.2f} s  {summary}", flush=True)
            if i:
                times[side].append(dt)
finally:
    TARGET.write_text(ORIGINAL)

print(f"\n{'side':<15} {'median s':>9} {'min':>7} {'max':>7}  runs")
for side, xs in times.items():
    print(f"{side:<15} {statistics.median(xs):>9.3f} {min(xs):>7.3f} {max(xs):>7.3f}  {len(xs)}")
