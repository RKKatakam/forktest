"""Every runner on every corpus project: counts and wall time, interleaved.

usage: python bench/corpus.py CORPUS_DIR FORKTEST_BIN ROUNDS [PROJECT ...]

CORPUS_RUNNERS=pytest,rpytest limits the runners.

Each project lives in CORPUS_DIR/<name> as a git checkout with its own .venv
holding pytest, pytest-xdist and rtest. rpytest and rpytest-daemon are looked
up in CORPUS_DIR/.tools/bin. Per round, per project:

  full   every runner from a cold state (forktest store and rpytest daemon gone)
  noop   every runner again with nothing changed
  edit   a no-op statement inserted at the top of one function, then every runner

`forktest --all` runs every test from the store `forktest` just warmed, which is
the fair full-suite comparison with pytest -n auto.

The edit is reverted with git checkout after each round. Counts come from each
runner's final summary line; a runner whose counts differ from pytest's in the
full scenario is flagged, since its time then measures different work.
"""

import os
import re
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

CORPUS = Path(sys.argv[1])
FORKTEST = sys.argv[2]
ROUNDS = int(sys.argv[3])
ONLY = set(sys.argv[4:])
TOOLS = CORPUS / ".tools/bin"


@dataclass
class Project:
    name: str
    edit_file: str
    edit_def: str
    args: list[str] = field(default_factory=list)
    python: str = ".venv/bin/python"


PROJECTS = [
    Project("demo", "billing/tax.py", "vat_for"),
    Project("jaxdemo", "model/layers.py", "dense"),
    Project("more-itertools", "more_itertools/more.py", "chunked"),
    Project("attrs", "src/attr/_funcs.py", "asdict"),
    Project("click", "src/click/utils.py", "echo"),
    Project("flask", "src/flask/helpers.py", "url_for"),
    Project("rich", "rich/text.py", "from_markup"),
    Project("pandas", "pandas/core/frame.py", "memory_usage", ["pandas/tests/frame"]),
]

ALL_RUNNERS = {
    "pytest": lambda p: [p.python, "-m", "pytest", "-q", *p.args],
    "pytest -n auto": lambda p: [p.python, "-m", "pytest", "-q", "-n", "auto", *p.args],
    "rtest -n auto": lambda p: [str(Path(p.python).parent / "rtest"), "-n", "auto", *p.args],
    "rpytest": lambda p: [str(TOOLS / "rpytest"), "-q", *p.args],
    "forktest": lambda p: [FORKTEST, "-q", *p.args],
    "forktest --all": lambda p: [FORKTEST, "-q", "--all", *p.args],
}

RUNNERS = {k: v for k, v in ALL_RUNNERS.items() if k in os.environ.get("CORPUS_RUNNERS", ",".join(ALL_RUNNERS)).split(",")}

COUNT = re.compile(r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?)\b")


def summary(output: str) -> dict[str, int]:
    for line in reversed(output.splitlines()):
        found = COUNT.findall(line)
        if found and re.search(r" in [\d.]+s", line):
            return {kind.rstrip("s") if kind.startswith("error") else kind: int(n) for n, kind in found}
    return {}


def run(project: Project, runner: str, root: Path) -> tuple[float, dict[str, int], int]:
    env = {**os.environ, "VIRTUAL_ENV": str(root / ".venv"), "PATH": f"{root / '.venv/bin'}:{TOOLS}:{os.environ['PATH']}"}
    env.pop("PYTHONPATH", None)
    t0 = time.perf_counter()
    proc = subprocess.run(RUNNERS[runner](project), cwd=root, env=env, capture_output=True, text=True, timeout=900)
    dt = time.perf_counter() - t0
    counts = summary(proc.stdout + proc.stderr)
    if not counts and proc.returncode not in (0, 1, 5):
        sys.stderr.write(f"{project.name} {runner}: exit {proc.returncode}\n{(proc.stdout + proc.stderr)[-1500:]}\n")
    return dt, counts, proc.returncode


def cold(runner: str, root: Path) -> None:
    if runner == "forktest":
        for db in (root / ".forktest").glob("forktest.db*"):
            db.unlink()
    if runner == "rpytest":
        # One daemon serves every project through a global socket, and --daemon-stop needs a pid file it can lose.
        subprocess.run([str(TOOLS / "rpytest"), "--daemon-stop"], cwd=root, capture_output=True)
        subprocess.run(["pkill", "-f", "rpytest-daemon"], capture_output=True)
        Path("/tmp/rpytest.sock").unlink(missing_ok=True)


def edit(root: Path, project: Project, variant: int) -> None:
    path = root / project.edit_file
    lines = path.read_text().splitlines(keepends=True)
    starts = [i for i, line in enumerate(lines) if re.match(rf"\s*def {project.edit_def}\(", line)]
    assert len(starts) == 1, f"{project.name}: {len(starts)} defs of {project.edit_def}"
    i, depth = starts[0], 0
    while True:
        depth += lines[i].count("(") + lines[i].count("[") - lines[i].count(")") - lines[i].count("]")
        if depth <= 0 and lines[i].rstrip().endswith(":"):
            break
        i += 1
    indent = re.match(r"\s*", lines[starts[0]]).group(0) + "    "
    lines.insert(i + 1, f"{indent}_forktest_corpus_edit = {variant}\n")
    path.write_text("".join(lines))


def revert(root: Path) -> None:
    subprocess.run(["git", "checkout", "--", "."], cwd=root, check=True)


def main() -> None:
    projects = [p for p in PROJECTS if not ONLY or p.name in ONLY]
    times: dict[tuple[str, str, str], list[float]] = {}
    counts: dict[tuple[str, str, str], dict[str, int]] = {}
    for rnd in range(ROUNDS):
        for project in projects:
            root = CORPUS / project.name
            if not (root / project.python).exists():
                continue
            revert(root)
            for scenario in ("full", "noop", "edit"):
                if scenario == "edit":
                    edit(root, project, rnd)
                for runner in RUNNERS:
                    if scenario == "full" and runner != "forktest --all":
                        cold(runner, root)
                    dt, c, code = run(project, runner, root)
                    times.setdefault((project.name, scenario, runner), []).append(dt)
                    counts[(project.name, scenario, runner)] = c
            revert(root)
            for runner in ("rpytest",):
                cold(runner, root)
            print(f"round {rnd + 1}: {project.name} done", flush=True)

    for project in projects:
        if not any(k[0] == project.name for k in times):
            continue
        print(f"\n## {project.name}")
        print(f"{'runner':16} {'full s':>8} {'noop s':>8} {'edit s':>8}  full counts / edit counts")
        reference = counts.get((project.name, "full", "pytest"))
        for runner in RUNNERS:
            cells = [statistics.median(times[(project.name, s, runner)]) for s in ("full", "noop", "edit")]
            full = counts[(project.name, "full", runner)]
            flag = "" if reference is None or full == reference else "  MISMATCH vs pytest"
            print(
                f"{runner:16} {cells[0]:8.2f} {cells[1]:8.2f} {cells[2]:8.2f}  {full} / {counts[(project.name, 'edit', runner)]}{flag}"
            )


main()
