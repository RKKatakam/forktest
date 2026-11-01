"""Interleaved A/B timing of two forktest binaries on one project, each on its own copy so both see every edit.

usage: python bench/bench_bins.py BIN_A BIN_B SOURCE EDIT_FILE ANCHOR_LINE ROUNDS [--json] [-- PYTEST_ARGS...]

SOURCE holds a .venv with the project and pytest installed; each side runs on a copy with that .venv linked in.
Each round inserts a no-op statement above ANCHOR_LINE in both copies, then times each side's edit run and each
side's --all, the order of the sides alternating by round. A warm-up round fills both stores and is not counted.
Runs print the human output unless --json; each run's last summary line is shown so parity is visible, and a run
whose exit code is not 0 or 1 counts as an error.
"""

import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

args = sys.argv[1:]
extra = args[args.index("--") + 1 :] if "--" in args else []
args = args[: args.index("--")] if "--" in args else args
json_mode = "--json" in args
args = [a for a in args if a != "--json"]
bin_a, bin_b, source, edit_file, anchor, rounds = args
SOURCE = Path(source).resolve()
ORIGINAL = (SOURCE / edit_file).read_text()
assert ORIGINAL.count(anchor) == 1, "anchor must appear exactly once"
indent = re.match(r"\s*", anchor).group(0)
IGNORE = shutil.ignore_patterns(".venv", ".forktest", ".pytest_cache", "__pycache__", ".git")


def copy(side: str) -> Path:
    dest = Path(tempfile.mkdtemp(prefix=f"bench-{side}-")) / SOURCE.name
    shutil.copytree(SOURCE, dest, ignore=IGNORE, symlinks=True)
    (dest / ".venv").symlink_to(SOURCE / ".venv")
    return dest


SIDES = {"A": (bin_a, copy("A")), "B": (bin_b, copy("B"))}
errors = 0


def timed(side: str, more: list[str]) -> tuple[float, str]:
    global errors
    binary, root = SIDES[side]
    cmd = [binary, "--no-daemon", *(["--json"] if json_mode else []), *more, *extra]
    env = {**os.environ, "FORKTEST_PYTHON": str(root / ".venv/bin/python")}
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True, env=env)
    dt = time.perf_counter() - t0
    if proc.returncode not in (0, 1):
        errors += 1
        sys.stderr.write(proc.stderr[-2000:])
    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    return dt, f"exit {proc.returncode} · {lines[-1].strip('= ')[:70] if lines else ''}"


times: dict[str, list[float]] = {}
for i in range(int(rounds) + 1):
    for _, root in SIDES.values():
        (root / edit_file).write_text(ORIGINAL.replace(anchor, f"{indent}_forktest_bench_edit = {i % 2}\n{anchor}", 1))
    print(f"round {i}{' (warm-up)' if i == 0 else ''}", flush=True)
    order = ["A", "B"] if i % 2 else ["B", "A"]
    for scenario, more in (("edit", []), ("--all", ["--all"])):
        for side in order:
            dt, summary = timed(side, more)
            name = f"{side} {scenario}"
            print(f"  {name:<10} {dt:6.3f} s  {summary}", flush=True)
            if i:
                times.setdefault(name, []).append(dt)

print(f"\n{'side':<10} {'median s':>9} {'min':>7} {'max':>7}  runs   (A={bin_a}, B={bin_b}, errors={errors})")
for name in sorted(times, key=lambda n: (n.split()[1], n)):
    xs = times[name]
    print(f"{name:<10} {statistics.median(xs):>9.3f} {min(xs):>7.3f} {max(xs):>7.3f}  {len(xs)}")
