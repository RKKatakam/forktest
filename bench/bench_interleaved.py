"""Interleaved pytest vs forktest timing on a real project, streamed to a live log.

usage: python bench/bench_interleaved.py ROOT TESTPATH EDIT_FILE ANCHOR_LINE ROUNDS LOG [--skip-serial]

ROOT must hold a .venv with the project, pytest-xdist and forktest installed.
Each round inserts a no-op statement above ANCHOR_LINE (a unique line inside
the function to edit), then times forktest (affected), pytest -n auto,
forktest --all and plain pytest, in that order. `tail -f LOG` to watch.
"""

import json
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

root, testpath, edit_file, anchor, rounds, log_path = sys.argv[1:7]
skip_serial = "--skip-serial" in sys.argv
ROOT = Path(root)
PY = str(ROOT / ".venv/bin/python")
FORKTEST = str(ROOT / ".venv/bin/forktest")
TARGET = ROOT / edit_file
ORIGINAL = TARGET.read_text()
assert ORIGINAL.count(anchor) == 1, "anchor must appear exactly once"
indent = re.match(r"\s*", anchor).group(0)
log = open(log_path, "a", buffering=1)


def say(msg: str) -> None:
    log.write(f"[{time.strftime('%H:%M:%S')}] {msg}\n")


def edited(variant: int) -> str:
    return ORIGINAL.replace(anchor, f"{indent}_forktest_bench_edit = {variant}\n{anchor}", 1)


def stream(label: str, cmd: list[str], on_line) -> float:
    say(f"── {label}: {' '.join(Path(c).name if c.startswith('/') else c for c in cmd)}")
    t0 = time.perf_counter()
    proc = subprocess.Popen(cmd, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    for line in proc.stdout:
        on_line(line.rstrip("\n"))
    proc.wait()
    dt = time.perf_counter() - t0
    say(f"   {label} finished in {dt:.2f} s (exit {proc.returncode})")
    return dt


def pytest_side(label: str, extra: list[str], summary: dict) -> float:
    def on_line(line: str) -> None:
        if re.search(r"\d+ (passed|failed)", line):
            summary[label] = line.strip("= ")
            say(f"   {line.strip('= ')}")
        elif line.startswith(("FAILED", "ERROR")):
            say(f"   {line[:160]}")
        elif line and set(line.split("[")[0].strip()) <= set(".sxFEX") and "%]" in line:
            log.write("   " + line + "\n")

    return stream(label, [PY, "-m", "pytest", "-q", "-p", "no:cacheprovider", "-p", "no:randomly", *extra, testpath], on_line)


def forktest_side(label: str, extra: list[str], summary: dict, selected: list) -> float:
    def on_line(line: str) -> None:
        if not line.startswith("{"):
            if "INTERNALERROR" in line or "Traceback" in line:
                say(f"   ! {line[:200]}")
            return
        e = json.loads(line)
        if e["event"] == "selection":
            selected.append(e["selected"])
            say(f"   selected {e['selected']:,} of {e['total']:,} in {e['duration_ms']} ms · reasons {e['reasons']}")
        elif e["event"] in ("fail", "error", "collect_error", "miss"):
            say(f"   ✗ {e['event']}: {e.get('test') or e.get('nodeid')}")
        elif e["event"] == "done":
            summary[label] = {k: e[k] for k in ("passed", "failed", "errors", "skipped", "xfailed", "xpassed") if k in e}
            say(f"   done: {summary[label]} in {e['duration_ms']} ms")

    return stream(label, [FORKTEST, "--no-daemon", "--json", *extra, testpath], on_line)


sides = ["forktest (edit)", "pytest -n auto", "forktest --all"] + ([] if skip_serial else ["pytest"])
times: dict[str, list[float]] = {s: [] for s in sides}
selected: list[int] = []
summaries: list[dict] = []
try:
    for i in range(int(rounds)):
        say(f"════ round {i + 1}/{rounds}: editing {edit_file} ════")
        TARGET.write_text(edited(i % 2))
        s: dict = {}
        times["forktest (edit)"].append(forktest_side("forktest (edit)", [], s, selected))
        times["pytest -n auto"].append(pytest_side("pytest -n auto", ["-n", "auto"], s))
        times["forktest --all"].append(forktest_side("forktest --all", ["--all"], s, []))
        if not skip_serial:
            times["pytest"].append(pytest_side("pytest", [], s))
        summaries.append(s)
finally:
    TARGET.write_text(ORIGINAL)

say("════ results ════")
lines = [f"{'side':<18} {'median s':>9} {'min':>8} {'max':>8}  runs"]
for side, xs in times.items():
    lines.append(f"{side:<18} {statistics.median(xs):>9.2f} {min(xs):>8.2f} {max(xs):>8.2f}  {len(xs)}")
lines.append(f"forktest selected per round: {selected}")
for line in lines:
    say(line)
print("\n".join(lines))
for i, s in enumerate(summaries):
    print(f"round {i + 1}: {s}")
