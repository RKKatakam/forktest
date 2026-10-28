"""What runs inside a forked worker: dependency recording and the item loop."""

from __future__ import annotations

import json
import os
import signal
import sys
from collections.abc import Callable, Sequence
from dataclasses import asdict
from typing import Any, NoReturn

from _pytest.reports import TestReport

import forktest
from forktest.events import Frame, TestResult
from forktest.selection import is_excluded


class ProjectFiles:
    """Decides whether an absolute path is project code, with a per-path cache."""

    def __init__(self, root: str) -> None:
        self.root = os.path.realpath(root).rstrip("/") + "/"
        own = os.path.dirname(os.path.realpath(forktest.__file__))
        self.excluded_prefixes = tuple(
            p.rstrip("/") + "/" for p in {sys.prefix, sys.base_prefix, sys.exec_prefix, own}
        )
        self._cache: dict[str, str | None] = {}

    def rel(self, path: str) -> str | None:
        if path in self._cache:
            return self._cache[path]
        rel: str | None = None
        if os.path.isabs(path):
            real = os.path.realpath(path)
            if real.startswith(self.root) and not real.startswith(self.excluded_prefixes):
                candidate = real[len(self.root):]
                if not is_excluded(candidate):
                    rel = candidate
        self._cache[path] = rel
        return rel

    def __call__(self, path: str) -> bool:
        return self.rel(path) is not None


class Recorder:
    """Per-test record of project functions that ran and project files that were read.

    ``PY_START`` fires once per code object between ``restart_events`` calls
    because the callback returns ``DISABLE``, so steady-state overhead is one
    callback per distinct function per test.
    """

    def __init__(self, project: ProjectFiles) -> None:
        self.project = project
        self.hits: set[tuple[str, str]] = set()
        self.reads: set[str] = set()
        self.tool_id: int | None = None
        self.recording = False
        self._audit_installed = False

    def install_audit_hook(self) -> None:
        if not self._audit_installed:
            sys.addaudithook(self._on_open)
            self._audit_installed = True

    def start(self) -> None:
        mon = sys.monitoring
        for tool_id in (3, 4, mon.COVERAGE_ID, mon.PROFILER_ID):
            if mon.get_tool(tool_id) is None:
                mon.use_tool_id(tool_id, "forktest")
                self.tool_id = tool_id
                break
        else:
            raise RuntimeError("no free sys.monitoring tool id")
        mon.register_callback(self.tool_id, mon.events.PY_START, self._on_start)
        mon.set_events(self.tool_id, mon.events.PY_START)

    def stop(self) -> None:
        if self.tool_id is not None:
            sys.monitoring.set_events(self.tool_id, 0)
            sys.monitoring.free_tool_id(self.tool_id)
            self.tool_id = None
        self.recording = False

    def begin(self) -> None:
        self.hits = set()
        self.reads = set()
        self.recording = True
        sys.monitoring.restart_events()

    def end(self) -> tuple[set[tuple[str, str]], set[str]]:
        self.recording = False
        hits, reads = self.hits, self.reads
        self.hits, self.reads = set(), set()
        return hits, reads

    def _on_start(self, code: Any, offset: int) -> Any:
        filename = code.co_filename
        if self.project(filename):
            self.hits.add((filename, code.co_qualname))
        return sys.monitoring.DISABLE

    def _on_open(self, event: str, args: tuple[Any, ...]) -> None:
        if event != "open" or not self.recording:
            return
        path, mode, flags = args
        if not isinstance(path, str) or path.endswith((".py", ".pyc")):
            return
        if mode is None:
            if flags & (os.O_WRONLY | os.O_RDWR):
                return
        elif "r" not in mode or "+" in mode:
            return
        if not os.path.isabs(path):
            try:
                path = os.path.join(os.getcwd(), path)
            except OSError:
                return
        if self.project(path):
            self.reads.add(path)


def result_of(nodeid: str, reports: Sequence[TestReport], project: ProjectFiles) -> TestResult:
    duration = sum(r.duration for r in reports)
    failed = [r for r in reports if r.failed]
    if failed:
        first = failed[0]
        outcome = "failed" if first.when == "call" else "error"
        message, file, line, trace = failure_details(first, project)
        for extra in failed[1:]:
            message += f"\n--- {extra.when} ---\n" + failure_details(extra, project)[0]
        return TestResult(nodeid, outcome, duration, file, line, message, trace)
    skipped = [r for r in reports if r.skipped]
    if skipped:
        lr = skipped[0].longrepr
        message = lr[2] if isinstance(lr, tuple) else str(lr)
        return TestResult(nodeid, "skipped", duration, message=message)
    return TestResult(nodeid, "passed", duration)


def failure_details(report: TestReport, project: ProjectFiles) -> tuple[str, str | None, int | None, list[Frame]]:
    lr = report.longrepr
    crash = getattr(lr, "reprcrash", None)
    message = crash.message if crash is not None else str(lr)
    if message.startswith("AssertionError: assert "):
        message = message[len("AssertionError: "):]
    trace: list[Frame] = []
    for filename, lineno, fn in getattr(report, "forktest_trace", ()):
        rel = project.rel(filename)
        if rel is not None:
            trace.append({"file": rel, "line": lineno, "fn": fn})
    if trace:
        return message, trace[-1]["file"], trace[-1]["line"], trace
    if crash is not None:
        rel = project.rel(str(crash.path))
        return message, rel, crash.lineno if rel else None, trace
    return message, None, None, trace


def run_items(items: Sequence[Any], send: Callable[[dict[str, Any]], None], recorder: Recorder, project: ProjectFiles) -> None:
    from _pytest.runner import runtestprotocol

    for i, item in enumerate(items):
        nextitem = items[i + 1] if i + 1 < len(items) else None
        recorder.begin()
        reports = runtestprotocol(item, nextitem=nextitem, log=False)
        hits, reads = recorder.end()
        send(
            {
                "type": "result",
                "result": asdict(result_of(item.nodeid, reports, project)),
                "hits": sorted(hits),
                "reads": sorted(reads),
            }
        )


def worker_main(
    items: Sequence[Any],
    write_fd: int,
    recorder: Recorder,
    project: ProjectFiles,
    reinit_capture: Callable[[], None],
    nice: bool,
) -> NoReturn:
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    out = os.fdopen(write_fd, "wb")

    def send(msg: dict[str, Any]) -> None:
        out.write(json.dumps(msg).encode() + b"\n")
        out.flush()

    try:
        if nice:
            os.nice(10)
        reinit_capture()
        recorder.start()
        run_items(items, send, recorder, project)
        send({"type": "done"})
    except BaseException as exc:  # noqa: BLE001 - the parent must learn why the worker stopped
        try:
            send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        except Exception:  # noqa: BLE001
            pass
    finally:
        try:
            out.flush()
        finally:
            os._exit(0)
