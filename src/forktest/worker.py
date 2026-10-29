"""What runs inside a forked worker: dependency recording and the item loop."""

from __future__ import annotations

import json
import os
import signal
import sys
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import asdict
from typing import Any, NoReturn

from _pytest.reports import TestReport

import forktest
from forktest.events import Frame, TestResult
from forktest.selection import is_excluded
from forktest.store import Dep, Deps, dep_hash, deps_digest


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
    if skipped and hasattr(skipped[0], "wasxfail"):
        return TestResult(nodeid, "xfailed", duration, message=skipped[0].wasxfail or "xfail")
    if skipped:
        lr = skipped[0].longrepr
        message = lr[2] if isinstance(lr, tuple) else str(lr)
        return TestResult(nodeid, "skipped", duration, message=message)
    if any(hasattr(r, "wasxfail") for r in reports):
        return TestResult(nodeid, "xpassed", duration)
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


class _ReportCollector:
    def __init__(self) -> None:
        self.reports: list[TestReport] = []

    def pytest_runtest_logreport(self, report: TestReport) -> None:
        self.reports.append(report)


class DepEncoder:
    """Worker side of the dependency wire format.

    A test whose dependency set has the digest the store already holds sends
    nothing. Otherwise each dependency goes over the pipe once per worker, as
    ``keys``, and a test's set is a list of those keys' positions.
    """

    def __init__(self, deps_of: Callable[[Any, set[tuple[str, str]], set[str]], list[Dep]], previous: Mapping[str, str]) -> None:
        self.deps_of = deps_of
        self.previous = previous
        self.ids: dict[Dep, int] = {}
        self.hashes = _Hashes()

    def encode(self, item: Any, hits: set[tuple[str, str]], reads: set[str]) -> dict[str, Any]:
        deps = self.deps_of(item, hits, reads)
        digest = deps_digest(deps, self.hashes.__getitem__)
        if self.previous.get(item.nodeid) == digest:
            return {}
        new = [d for d in deps if d not in self.ids]
        for d in new:
            self.ids[d] = len(self.ids)
        return {"digest": digest, "keys": new, "deps": [self.ids[d] for d in deps]}


class _Hashes(dict[Dep, int]):
    def __missing__(self, dep: Dep) -> int:
        h = self[dep] = dep_hash(dep)
        return h


class DepDecoder:
    """Parent side of ``DepEncoder``, one per worker."""

    def __init__(self) -> None:
        self.keys: list[Dep] = []

    def decode(self, msg: Mapping[str, Any]) -> Deps | None:
        if "digest" not in msg:
            return None
        self.keys.extend(Dep(*k) for k in msg["keys"])
        return Deps([self.keys[i] for i in msg["deps"]], msg["digest"])


def run_items(
    items: Sequence[Any],
    send: Callable[[dict[str, Any]], None],
    recorder: Recorder,
    project: ProjectFiles,
    encode: Callable[[Any, set[tuple[str, str]], set[str]], dict[str, Any]],
) -> None:
    if not items:
        return
    collector = _ReportCollector()
    pluginmanager = items[0].config.pluginmanager
    pluginmanager.register(collector)
    try:
        for i, item in enumerate(items):
            nextitem = items[i + 1] if i + 1 < len(items) else None
            collector.reports = []
            started = time.perf_counter()
            recorder.begin()
            # The full hook, not runtestprotocol: plugins such as warnings (filterwarnings marks) wrap it.
            item.ihook.pytest_runtest_protocol(item=item, nextitem=nextitem)
            hits, reads = recorder.end()
            result = asdict(result_of(item.nodeid, collector.reports, project))
            deps = encode(item, hits, reads)
            # What the scheduler should plan with: the reports' durations leave out building them, which
            # for a failure or an xfail (formatting the traceback) can be 100 times the test itself.
            cost = time.perf_counter() - started
            send({"type": "result", "result": result, "cost": cost, **deps})
    finally:
        pluginmanager.unregister(collector)


def worker_main(
    items: Sequence[Any],
    write_fd: int,
    recorder: Recorder,
    project: ProjectFiles,
    reinit_capture: Callable[[], None],
    nice: bool,
    encode: Callable[[Any, set[tuple[str, str]], set[str]], dict[str, Any]],
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
        run_items(items, send, recorder, project, encode)
        send({"type": "done"})
    except BaseException as exc:
        try:
            send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        except Exception:
            pass
    finally:
        try:
            out.flush()
        finally:
            os._exit(0)
