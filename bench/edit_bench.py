"""Interleaved edit-to-result timing of two forktest binaries on one project.

usage: python bench/edit_bench.py MODE ROOT EDIT_FILE ANCHOR ROUNDS BIN_A BIN_B [PYTEST_ARGS...]

Each run first appends a line of a new length under ANCHOR (a whole line of EDIT_FILE, indentation included), so
every run meets the same kind of edit. The sides alternate their order every round. EDIT_FILE is restored with
`git checkout` at the end.

- oneshot: times `BIN --no-daemon --json` right after the edit.
- building: starts BIN's daemon, makes the edit, waits until the daemon reports its zygote building, then times
  a client run, so the run meets the rebuild. The daemon stops after each run.

Every run prints the load average, the selection, the outcome counts and the FORKTEST_LOG collection line.
"""

import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

mode, root, edit_file, anchor, rounds, bin_a, bin_b, *pytest_args = sys.argv[1:]
root = Path(root)
assert mode in ("oneshot", "building")
edits = 0


def load() -> str:
    return subprocess.run(["uptime"], capture_output=True, text=True).stdout.split("load averages:")[-1].strip()


def run(binary: str, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run([binary, *args], cwd=root, capture_output=True, text=True, env={**os.environ, "FORKTEST_LOG": "1"})


def edit() -> None:
    global edits
    edits += 1
    path = root / edit_file
    lines = path.read_text().splitlines(keepends=True)
    i = next(i for i, line in enumerate(lines) if line.rstrip("\n") == anchor)
    indent = anchor[: len(anchor) - len(anchor.lstrip())]
    if lines[i + 1].lstrip().startswith("_forktest_bench"):
        del lines[i + 1]
    lines.insert(i + 1, f"{indent}_forktest_bench = {'1' * edits}\n")
    path.write_text("".join(lines))


def status(binary: str) -> dict[str, str]:
    proc = run(binary, "daemon", "status")
    return dict(line.split(": ", 1) for line in proc.stdout.splitlines()) if proc.returncode == 0 else {}


def wait_zygote(binary: str, state: str, timeout: float = 120) -> None:
    deadline = time.monotonic() + timeout
    while status(binary).get("zygote") != state:
        assert time.monotonic() < deadline, f"no {state} zygote"
        time.sleep(0.005)


def timed(binary: str, *args: str) -> tuple[float, subprocess.CompletedProcess]:
    started = time.perf_counter()
    proc = run(binary, "--json", *args, *pytest_args)
    return time.perf_counter() - started, proc


def summary(proc: subprocess.CompletedProcess) -> str:
    events = [json.loads(line) for line in proc.stdout.splitlines() if line.startswith("{")]
    sel = next((e for e in events if e["event"] == "selection"), {})
    done = next((e for e in events if e["event"] == "done"), {})
    collected = [line for line in proc.stderr.splitlines() if "collected" in line or "served by" in line]
    return (
        f"selected={sel.get('selected')} passed={done.get('passed')} failed={done.get('failed')} "
        f"errors={done.get('errors')} | {' | '.join(collected)}"
    )


def oneshot(binary: str) -> float:
    edit()
    elapsed, proc = timed(binary, "--no-daemon")
    print(f"    {summary(proc)}")
    return elapsed


def building(binary: str) -> float:
    assert run(binary, "daemon", "start", *pytest_args).returncode == 0
    try:
        wait_zygote(binary, "ready")
        timed(binary)
        wait_zygote(binary, "ready")
        edit()
        wait_zygote(binary, "building")
        elapsed, proc = timed(binary)
        print(f"    {summary(proc)}")
        return elapsed
    finally:
        run(binary, "daemon", "stop")


sides = {"A": bin_a, "B": bin_b}
times: dict[str, list[float]] = {"A": [], "B": []}
try:
    for r in range(int(rounds)):
        for name in ("A", "B") if r % 2 == 0 else ("B", "A"):
            before = load()
            elapsed = (oneshot if mode == "oneshot" else building)(sides[name])
            times[name].append(elapsed)
            print(f"round {r + 1} {name}: {elapsed * 1000:7.0f} ms  load {before}")
finally:
    subprocess.run(["git", "checkout", "--", edit_file], cwd=root, check=True)
print(f"\n{'side':6}{'median ms':>10}{'min':>8}{'max':>8}  runs")
for name, ts in times.items():
    ms = [t * 1000 for t in ts]
    print(f"{name:6}{statistics.median(ms):10.0f}{min(ms):8.0f}{max(ms):8.0f}  {len(ms):4}  {sides[name]}")
