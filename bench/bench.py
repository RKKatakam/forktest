"""Time pytest against forktest on a project: cold full run, incremental after a
one-function edit, and the same edit against a warm daemon.

    uv run python bench/bench.py [--project examples/demo] [--runs 5]

The project is copied to a temp dir first. Sides alternate within each round
so warm caches and drift favour neither. Every run's pass/fail counts are
checked against the expected ones, so a fast run that did no work fails the
bench instead of winning it.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_EDIT = ("billing/tax.py", "    return amount.quantize(quantum, rounding=ROUND_HALF_UP)\n")
FORKTEST = [sys.executable, "-m", "forktest"]
PYTEST = [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider"]


class Bench:
    def __init__(self, project: Path, edit_file: str, edit_line: str, runs: int, pytest_args: list[str]) -> None:
        self.src = project
        self.runs = runs
        self.edit_file = edit_file
        self.edit_line = edit_line
        self.pytest_args = pytest_args
        self.tmp = Path(tempfile.mkdtemp(prefix="forktest-bench-"))
        self.root = self.tmp / project.name
        shutil.copytree(project, self.root, ignore=shutil.ignore_patterns(".forktest", ".pytest_cache", "__pycache__", ".git"))
        self.toggle = 0
        self.rows: list[tuple[str, str, list[float], str]] = []

    def sh(self, cmd: list[str]) -> tuple[float, subprocess.CompletedProcess[str]]:
        t0 = time.perf_counter()
        proc = subprocess.run(cmd, cwd=self.root, capture_output=True, text=True)
        return time.perf_counter() - t0, proc

    def edit(self) -> None:
        """Toggle a marker line inside the function body so every round sees a real, different change."""
        path = self.root / self.edit_file
        text = path.read_text()
        indent = re.match(r"\s*", self.edit_line).group()
        text = re.sub(rf"{re.escape(indent)}_bench_touch = \d+\n", "", text)
        assert self.edit_line in text, f"edit anchor not found in {self.edit_file}"
        marker = f"{indent}_bench_touch = {(1, 22)[self.toggle]}\n"
        path.write_text(text.replace(self.edit_line, marker + self.edit_line, 1))
        self.toggle ^= 1

    @staticmethod
    def pytest_counts(out: str) -> str:
        m = re.search(r"(\d+ passed|\d+ failed)[^\n]*", out)
        return m.group(0).split(" in ")[0] if m else "?"

    @staticmethod
    def forktest_counts(out: str) -> str:
        done = sel = None
        for line in out.splitlines():
            d = json.loads(line)
            if d["event"] == "done":
                done = d
            elif d["event"] == "selection":
                sel = d
        if not done:
            return "no done event"
        tail = f", {sel['selected']}/{sel['total']} selected" if sel else ""
        return f"{done['passed']} passed" + (f", {done['failed']} failed" if done["failed"] else "") + tail

    def measure(self, scenario: str, side: str, cmd: list[str], counts, before=None) -> None:
        times, seen = [], set()
        for _ in range(self.runs):
            if before:
                before()
            secs, proc = self.sh(cmd)
            if proc.returncode not in (0, 1):
                sys.exit(f"{side} failed ({proc.returncode}):\n{proc.stderr[-2000:]}")
            times.append(secs)
            seen.add(counts(proc.stdout))
        self.rows.append((scenario, side, times, " | ".join(sorted(seen))))

    def run(self) -> None:
        print(f"project: {self.src}  copy: {self.root}")
        print(f"python {sys.version.split()[0]}, {os.cpu_count()} cpus, load {os.getloadavg()[0]:.2f}, runs per side: {self.runs}\n")
        self.sh(PYTEST + self.pytest_args)
        _, seed = self.sh(FORKTEST + ["--no-daemon", "--all", "--json", *self.pytest_args])
        print("seed:", self.forktest_counts(seed.stdout))

        self.measure("interpreter + import pytest", "python -c 'import pytest'", [sys.executable, "-c", "import pytest"], lambda out: "n/a")
        self.measure("cold full run", "pytest", PYTEST + self.pytest_args, self.pytest_counts)
        self.measure("cold full run", "forktest --all", FORKTEST + ["--no-daemon", "--all", "--json", *self.pytest_args], self.forktest_counts)
        self.measure("one-function edit, cold process", "pytest (whole suite)", PYTEST + self.pytest_args, self.pytest_counts, before=self.edit)
        self.measure("one-function edit, cold process", "forktest (affected)", FORKTEST + ["--no-daemon", "--json", *self.pytest_args], self.forktest_counts, before=self.edit)

        _, start = self.sh(FORKTEST + ["daemon", "start", *self.pytest_args])
        if start.returncode:
            sys.exit("daemon did not start:\n" + start.stdout + start.stderr)
        self.wait_ready()
        try:
            self.measure("no edit, warm daemon", "forktest (0 selected)", FORKTEST + ["--json", *self.pytest_args], self.forktest_counts)
            self.measure("one-function edit, warm daemon", "forktest (affected, waits for zygote rebuild)", FORKTEST + ["--json", *self.pytest_args], self.forktest_counts, before=self.edit)
            self.measure("one-function edit, warm daemon", "forktest (affected, zygote already rebuilt)", FORKTEST + ["--json", *self.pytest_args], self.forktest_counts, before=lambda: (self.edit(), time.sleep(0.4), self.wait_ready()))
        finally:
            self.sh(FORKTEST + ["daemon", "stop"])
        self.report()
        shutil.rmtree(self.tmp, ignore_errors=True)

    def wait_ready(self) -> None:
        for _ in range(600):
            _, st = self.sh(FORKTEST + ["daemon", "status"])
            if "zygote: ready" in st.stdout:
                return
            time.sleep(0.05)
        sys.exit("zygote never became ready")

    def report(self) -> None:
        print(f"\n{'scenario':34} {'side':48} {'median':>8} {'min':>8} {'max':>8}  work checked")
        for scenario, side, times, counts in self.rows:
            print(f"{scenario:34} {side:48} {statistics.median(times):7.3f}s {min(times):7.3f}s {max(times):7.3f}s  {counts}")
        print("\nAll times are wall-clock seconds of one CLI invocation, median of the runs above, measured on this machine.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--project", default=str(HERE.parent / "examples" / "demo"))
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--edit-file", default=DEFAULT_EDIT[0], help="file holding the function to edit each round")
    parser.add_argument("--edit-line", default=DEFAULT_EDIT[1], help="exact source line inside that function body")
    parser.add_argument("pytest_args", nargs="*", help="extra arguments passed to both runners")
    args = parser.parse_args()
    Bench(Path(args.project).resolve(), args.edit_file, args.edit_line, args.runs, args.pytest_args).run()


if __name__ == "__main__":
    main()
