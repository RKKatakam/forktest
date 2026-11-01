"""Drives the Rust binary on scratch copies of the examples. Self-contained: imports nothing from the forktest package."""

import json
import os
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
DEMO = REPO / "examples" / "demo"
PYTHON = os.environ.get("FORKTEST_PYTHON") or str(REPO / ".venv" / "bin" / "python")


@pytest.fixture(scope="session")
def forktest_bin() -> str:
    if "FORKTEST_BIN" in os.environ:
        return os.environ["FORKTEST_BIN"]
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=REPO, check=True)
    return str(REPO / "target" / "release" / "forktest")


@dataclass
class Selection:
    changed: list
    selected: int
    total: int
    reasons: dict
    duration_ms: int = 0


@dataclass
class TestResult:
    nodeid: str
    outcome: str
    duration: float
    file: str | None = None
    line: int | None = None
    message: str | None = None
    trace: list = field(default_factory=list)

    __test__ = False


@dataclass
class CollectError:
    nodeid: str
    message: str


@dataclass
class Miss:
    nodeid: str
    message: str


@dataclass
class Verified:
    checked: int
    misses: int
    duration_ms: int


@dataclass
class Done:
    passed: int = 0
    failed: int = 0
    errors: int = 0
    skipped: int = 0
    xfailed: int = 0
    xpassed: int = 0
    duration_ms: int = 0


OUTCOMES = {"pass": "passed", "fail": "failed", "skip": "skipped", "error": "error", "xfail": "xfailed", "xpass": "xpassed"}


def parse(line: str):
    d = json.loads(line)
    kind = d.pop("event")
    if kind in OUTCOMES:
        return TestResult(
            nodeid=d["test"],
            outcome=OUTCOMES[kind],
            duration=d["duration_ms"] / 1000,
            file=d.get("file"),
            line=d.get("line"),
            message=d.get("message"),
            trace=d.get("trace", []),
        )
    if kind == "miss":
        return Miss(d["test"], d["message"])
    return {"selection": Selection, "collect_error": CollectError, "verified": Verified, "done": Done}[kind](**d)


def selection(events) -> Selection | None:
    return next((e for e in events if isinstance(e, Selection)), None)


def done(events) -> Done:
    return next(e for e in events if isinstance(e, Done))


def verified(events) -> Verified:
    return next(e for e in events if isinstance(e, Verified))


def failed(events) -> set[str]:
    return {e.nodeid for e in events if isinstance(e, TestResult) and e.outcome in ("failed", "error")}


class Project:
    def __init__(self, root: Path, binary: str, python: str = PYTHON) -> None:
        self.root = root
        self.binary = binary
        self.python = python
        self.stderr = ""

    def env(self, **extra: str) -> dict[str, str]:
        return {**os.environ, "FORKTEST_PYTHON": self.python, **extra}

    def run(self, *args: str, timeout: float = 120, **env: str) -> subprocess.CompletedProcess:
        proc = subprocess.run(
            [self.binary, *args], cwd=self.root, capture_output=True, text=True, timeout=timeout, env=self.env(**env)
        )
        self.stderr = proc.stderr
        return proc

    def forktest(self, *args: str, **env: str) -> tuple[list, int]:
        """Events of a `--json` run with FORKTEST_LOG on; the log lines are in `self.stderr`."""
        proc = self.run("--no-daemon", "--json", *args, FORKTEST_LOG="1", **env)
        assert "INTERNALERROR" not in proc.stderr and "forktest: error" not in proc.stderr, proc.stderr
        return [parse(line) for line in proc.stdout.splitlines()], proc.returncode

    def log(self, prefix: str) -> list[str]:
        return [line for line in self.stderr.splitlines() if line.startswith(f"forktest: {prefix}")]

    def pytest(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [self.python, "-m", "pytest", *args], cwd=self.root, capture_output=True, text=True, timeout=120
        )

    def pytest_counts(self, *args: str) -> dict[str, int]:
        summary = self.pytest("-q", "-p", "no:cacheprovider", *args).stdout.strip().splitlines()[-1]
        counts = {}
        for part in summary.split(" in ")[0].split(", "):
            n, word = part.split()
            counts[word] = int(n)
        return counts

    def edit(self, rel: str, old: str, new: str) -> None:
        """Replace text, requiring a length change so CPython never reuses a same-second .pyc."""
        path = self.root / rel
        text = path.read_text()
        assert old in text and len(old) != len(new)
        path.write_text(text.replace(old, new))

    def write(self, rel: str, text: str) -> None:
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


@pytest.fixture
def fresh(tmp_path, forktest_bin) -> Project:
    shutil.copytree(DEMO, tmp_path / "demo")
    return Project(tmp_path / "demo", forktest_bin)


@pytest.fixture
def demo(fresh) -> Project:
    events, rc = fresh.forktest()
    assert rc == 0 and selection(events).reasons == {"new": 16} and done(events).passed == 16
    return fresh
