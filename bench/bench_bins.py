"""Interleaved A/B timing of two forktest binaries on one project, each on its own copy so both see every edit.

usage: python bench/bench_bins.py BIN_A BIN_B SOURCE EDIT_FILE ANCHOR_LINE ROUNDS [--json] [--daemon] [-- PYTEST_ARGS...]

SOURCE holds a .venv with the project and pytest installed; each side runs on a copy with that .venv linked in.
Each round inserts a no-op statement above ANCHOR_LINE in both copies, then times each side's edit run and each
side's --all, the order of the sides alternating by round. With --daemon each side runs a daemon instead, and
each round times the edit run alone, once the daemon reports the zygote rebuilt after the edit. A warm-up round
fills both stores and is not counted. Runs print the human output unless --json; each run's last summary line is
shown so parity is visible, and a run whose exit code is not 0 or 1 counts as an error.
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
daemon = "--daemon" in args
args = [a for a in args if a not in ("--json", "--daemon")]
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
    cmd = [binary, *([] if daemon else ["--no-daemon"]), *(["--json"] if json_mode else []), *more, *extra]
    env = {**os.environ, "FORKTEST_PYTHON": str(root / ".venv/bin/python")}
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True, env=env)
    dt = time.perf_counter() - t0
    if proc.returncode not in (0, 1):
        errors += 1
        sys.stderr.write(proc.stderr[-2000:])
    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    return dt, f"exit {proc.returncode} · {lines[-1].strip('= ')[:70] if lines else ''}"


def zygote(side: str) -> str | None:
    binary, root = SIDES[side]
    env = {**os.environ, "FORKTEST_PYTHON": str(root / ".venv/bin/python")}
    proc = subprocess.run([binary, "daemon", "status"], cwd=root, capture_output=True, text=True, env=env)
    st = dict(line.split(": ", 1) for line in proc.stdout.splitlines()) if proc.returncode == 0 else {}
    return st.get("zygote_pid") if st.get("zygote") == "ready" else None


def wait_rebuilt(side: str, before: str | None) -> None:
    deadline = time.monotonic() + 60
    while zygote(side) in (None, "None", before):
        assert time.monotonic() < deadline, f"{side}: no ready zygote"
        time.sleep(0.01)


if daemon:
    for side, (binary, root) in SIDES.items():
        env = {**os.environ, "FORKTEST_PYTHON": str(root / ".venv/bin/python")}
        subprocess.run([binary, "daemon", "start", *extra], cwd=root, capture_output=True, env=env, check=True)
        wait_rebuilt(side, None)

times: dict[str, list[float]] = {}
try:
    for i in range(int(rounds) + 1):
        zygotes = {side: zygote(side) for side in SIDES} if daemon else {}
        for _, root in SIDES.values():
            # The edit changes length, so CPython never reuses a .pyc written in the same second.
            edit = f"{indent}_forktest_bench_edit = '{'x' * (1 + i % 2)}'\n{anchor}"
            (root / edit_file).write_text(ORIGINAL.replace(anchor, edit, 1))
        print(f"round {i}{' (warm-up)' if i == 0 else ''}", flush=True)
        order = ["A", "B"] if i % 2 else ["B", "A"]
        for scenario, more in (("edit", []),) if daemon else (("edit", []), ("--all", ["--all"])):
            for side in order:
                if daemon:
                    wait_rebuilt(side, zygotes[side])
                dt, summary = timed(side, more)
                name = f"{side} {scenario}"
                print(f"  {name:<10} {dt:6.3f} s  {summary}", flush=True)
                if i:
                    times.setdefault(name, []).append(dt)
finally:
    if daemon:
        for side, (binary, root) in SIDES.items():
            subprocess.run([binary, "daemon", "stop"], cwd=root, capture_output=True)

print(f"\n{'side':<10} {'median s':>9} {'min':>7} {'max':>7}  runs   (A={bin_a}, B={bin_b}, errors={errors})")
for name in sorted(times, key=lambda n: (n.split()[1], n)):
    xs = times[name]
    print(f"{name:<10} {statistics.median(xs):>9.3f} {min(xs):>7.3f} {max(xs):>7.3f}  {len(xs)}")
