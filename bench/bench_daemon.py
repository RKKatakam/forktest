"""Interleaved edit-to-result timing through a running daemon: the Rust forktest against the Python one.

usage: python bench/bench_daemon.py SOURCE EDIT_FILE OLD NEW EXPECT_FAILED ROUNDS [PYTHON]

SOURCE is copied twice, one copy per side, since both daemons own .forktest/daemon.sock. PYTHON (default: this
interpreter) runs the tests on both sides. The Rust side is $FORKTEST_BIN, the Python side the source tree in
$FORKTEST_PY_SRC. Each round flips EDIT_FILE between OLD and NEW (lengths must differ) and times, per side:

- cold: the write, then the client run at once, so the run meets the zygote rebuild the write triggers;
- warm: the write, then waiting until the daemon reports a new ready zygote, then timing the client run alone.

Sides alternate their order every round. Every run's failure count is checked against EXPECT_FAILED (with NEW in
place) or 0 (with OLD), so a run served by stale code counts as an error, not a time.
"""

import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

source, edit_file, old, new, expect_failed, rounds = sys.argv[1:7]
PYTHON = sys.argv[7] if len(sys.argv) > 7 else sys.executable
assert len(old) != len(new), "a same-length edit can reuse a stale .pyc"
SCRATCH = Path(tempfile.mkdtemp(prefix="forktest-bench-daemon-"))


class Side:
    def __init__(self, name: str, command: list[str], env: dict[str, str]) -> None:
        self.name = name
        self.root = SCRATCH / name
        shutil.copytree(source, self.root, ignore=shutil.ignore_patterns(".forktest", "__pycache__"))
        self.command = command
        self.env = {**os.environ, "FORKTEST_PYTHON": PYTHON, **env}
        self.edited = False
        self.times: dict[str, list[float]] = {"cold": [], "warm": []}
        self.errors = 0

    def run(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([*self.command, *args], cwd=self.root, capture_output=True, text=True, env=self.env)

    def status(self) -> dict[str, str]:
        proc = self.run("daemon", "status")
        return dict(line.split(": ", 1) for line in proc.stdout.splitlines()) if proc.returncode == 0 else {}

    def ready_zygote(self) -> str | None:
        st = self.status()
        return st.get("zygote_pid") if st.get("zygote") == "ready" else None

    def wait_ready(self, other_than: str | None) -> None:
        deadline = time.monotonic() + 60
        while (z := self.ready_zygote()) in (None, "None", "none", other_than):
            assert time.monotonic() < deadline, f"{self.name}: no ready zygote"
            time.sleep(0.01)

    def flip(self) -> None:
        path = self.root / edit_file
        text = path.read_text()
        a, b = (new, old) if self.edited else (old, new)
        assert text.count(a) == 1, f"{self.name}: {a!r} not in {edit_file}"
        path.write_text(text.replace(a, b))
        self.edited = not self.edited

    def client(self, kind: str, started: float) -> None:
        proc = self.run("--json")
        elapsed = time.perf_counter() - started
        done = [json.loads(line) for line in proc.stdout.splitlines() if line.startswith('{"event":"done"')]
        want = int(expect_failed) if self.edited else 0
        if not done or done[-1]["failed"] != want or proc.returncode not in (0, 1):
            self.errors += 1
            print(f"  {self.name} {kind}: ERROR, wanted {want} failed: {proc.stdout[-300:]} {proc.stderr[-300:]}")
            return
        self.times[kind].append(elapsed)
        print(f"  {self.name:6} {kind}: {elapsed * 1000:6.1f} ms  failed={done[-1]['failed']} passed={done[-1]['passed']}")

    def cold(self) -> None:
        started = time.perf_counter()
        self.flip()
        self.client("cold", started)

    def warm(self) -> None:
        before = self.ready_zygote()
        self.flip()
        self.wait_ready(other_than=before)
        self.client("warm", time.perf_counter())


sides = [
    Side("rust", [os.environ["FORKTEST_BIN"]], {}),
    Side("python", [PYTHON, "-m", "forktest"], {"PYTHONPATH": os.environ["FORKTEST_PY_SRC"]}),
]
try:
    for side in sides:
        assert side.run("daemon", "start").returncode == 0, side.name
        side.wait_ready(None)
        side.run("--json")
        side.wait_ready(None)
    for kind in ("cold", "warm"):
        for side in sides:
            getattr(side, kind)()
    for side in sides:
        side.times = {"cold": [], "warm": []}
    print("warm-up done")
    for r in range(int(rounds)):
        order = sides if r % 2 == 0 else sides[::-1]
        print(f"round {r + 1}")
        for kind in ("cold", "warm"):
            for side in order:
                time.sleep(0.5)
                getattr(side, kind)()
    print(f"\n{'side':8}{'kind':6}{'median ms':>10}{'min':>8}{'max':>8}  runs errors")
    for side in sides:
        for kind, times in side.times.items():
            ms = [t * 1000 for t in times]
            print(f"{side.name:8}{kind:6}{statistics.median(ms):10.1f}{min(ms):8.1f}{max(ms):8.1f}  {len(ms):4} {side.errors:6}")
finally:
    for side in sides:
        side.run("daemon", "stop")
    shutil.rmtree(SCRATCH, ignore_errors=True)
