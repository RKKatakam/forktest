"""The forktest command line."""

from __future__ import annotations

import argparse
import os
import sys
from collections.abc import Callable

from forktest.events import CollectError, Done, Event, Miss, Request, Selection
from forktest.reporters import HumanReporter, JsonlReporter, JunitReporter, Reporter

INI_FILES = ("pytest.ini", ".pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg")
INI_SECTIONS = {"pyproject.toml": "[tool.pytest", "tox.ini": "[pytest]", "setup.cfg": "[tool:pytest]"}


def find_root(start: str) -> str:
    """Approximate pytest's rootdir without importing pytest: ini file, then project file, then .git."""
    start = os.path.realpath(start)
    ancestors = []
    d = start
    while True:
        ancestors.append(d)
        parent = os.path.dirname(d)
        if parent == d:
            break
        d = parent
    for d in ancestors:
        for name in INI_FILES:
            path = os.path.join(d, name)
            if not os.path.isfile(path):
                continue
            section = INI_SECTIONS.get(name)
            if section is None:
                return d
            with open(path, encoding="utf-8", errors="replace") as f:
                if section in f.read():
                    return d
    for marker in ("pyproject.toml", "setup.py", ".git"):
        for d in ancestors:
            if os.path.exists(os.path.join(d, marker)):
                return d
    return start


def parse_args(argv: list[str]) -> tuple[argparse.Namespace, list[str]]:
    parser = argparse.ArgumentParser(
        prog="forktest",
        description="Run the pytest tests a change affects, forked from a warm zygote.",
        epilog="Unknown arguments pass through to pytest. Subcommands: watch, daemon start|stop|status.",
        allow_abbrev=False,
    )
    parser.add_argument("--all", action="store_true", help="run every collected test")
    parser.add_argument("--since", metavar="REF", help="tests affected by changes since the merge base with REF")
    parser.add_argument("--json", action="store_true", help="JSONL events on stdout")
    parser.add_argument("--junitxml", metavar="PATH", help="write a JUnit XML report")
    parser.add_argument("-n", "--workers", type=int, default=0, metavar="N", help="worker count (default: cpu count)")
    parser.add_argument("--strict-pytest", action="store_true", help="one process, one shared session")
    parser.add_argument("--verify-selection", action="store_true", help="also run the unselected tests and report misses")
    parser.add_argument("--why", metavar="NODEID", help="explain why a test would be selected now, without running")
    parser.add_argument("--no-daemon", action="store_true", help="never use a running daemon")
    opts, rest = parser.parse_known_args(argv)
    return opts, rest


class ExitStatus:
    """A reporter that only remembers what the exit code should be."""

    def __init__(self) -> None:
        self.done: Done | None = None
        self.total: int | None = None
        self.misses = 0
        self.collect_errors = 0

    def handle(self, event: Event) -> None:
        if isinstance(event, Done):
            self.done = event
        elif isinstance(event, Selection):
            self.total = event.total
        elif isinstance(event, Miss):
            self.misses += 1
        elif isinstance(event, CollectError):
            self.collect_errors += 1

    def close(self) -> None:
        pass

    def code(self, fallback: int) -> int:
        if self.done is None:
            return fallback or 3
        if self.collect_errors and self.total is None:
            return 2
        if self.done.failed or self.done.errors or self.misses:
            return 1
        if self.total == 0:
            return 5
        return 0


def reexec_for_fork_safety(argv: list[str]) -> None:
    """macOS may abort a forked child once Apple frameworks initialised; this env var disables that check."""
    if sys.platform == "darwin" and os.environ.get("OBJC_DISABLE_INITIALIZE_FORK_SAFETY") != "YES":
        env = {**os.environ, "OBJC_DISABLE_INITIALIZE_FORK_SAFETY": "YES"}
        os.execve(sys.executable, [sys.executable, "-m", "forktest", *argv], env)


def why_command(root: str, nodeid: str) -> int:
    from forktest.fingerprint import Fingerprints
    from forktest.selection import PRIORITY, ImportGraph, reasons_for
    from forktest.store import Store

    store = Store(os.path.join(root, ".forktest", "store.db"))
    try:
        view = store.view()
    finally:
        store.close()
    fps = Fingerprints(root)
    reasons = reasons_for(nodeid, view, fps.of, ImportGraph(root, fps), frozenset(), {})
    if not reasons:
        print(f"{nodeid}: not selected (no recorded dependency changed)")
        return 0
    reasons.sort(key=lambda r: PRIORITY.index(r[0]))
    print(f"{nodeid}: selected as {reasons[0][0]}")
    for reason, dep in reasons:
        if dep is None:
            print(f"  {reason}")
        else:
            what = dep.path if dep.qualname in ("<module>", "<file>") else f"{dep.path}::{dep.qualname}"
            print(f"  {reason}: {what} changed")
    return 0


def main(argv: list[str] | None = None) -> int:
    try:
        return _main(argv)
    except BrokenPipeError:
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        return 1


def _main(argv: list[str] | None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    from forktest import daemon

    if argv[:1] == ["watch"]:
        reexec_for_fork_safety(argv)
        return daemon.watch_command(argv[1:])
    if argv[:1] == ["daemon"]:
        if argv[1:2] == ["start"]:
            reexec_for_fork_safety(argv)
        return daemon.daemon_command(argv[1:])
    opts, pytest_args = parse_args(argv)
    root = find_root(os.getcwd())
    if opts.why:
        return why_command(root, opts.why)

    request = Request(
        mode="all" if opts.all else "since" if opts.since else "affected",
        since=opts.since,
        verify=opts.verify_selection,
        workers=opts.workers,
        strict=opts.strict_pytest,
        pytest_args=pytest_args,
    )
    reporters: list[Reporter] = [
        JsonlReporter(sys.stdout) if opts.json else HumanReporter(sys.stdout, expect_verify=opts.verify_selection)
    ]
    if opts.junitxml:
        reporters.append(JunitReporter(opts.junitxml))
    status = ExitStatus()
    reporters.append(status)

    def emit(event: Event) -> None:
        for r in reporters:
            r.handle(event)

    rc = 0
    if opts.no_daemon or not daemon.run_via_daemon(root, request, emit):
        reexec_for_fork_safety(argv)
        from forktest.zygote import run_oneshot

        rc = run_oneshot(request, pytest_args, emit)
    for r in reporters:
        r.close()
    return status.code(rc)
