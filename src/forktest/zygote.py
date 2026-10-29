"""The pytest plugin that takes over the run loop, and the runner behind it.

The process that called ``pytest.main`` with this plugin is the zygote. It
holds the collected session; each request forks workers from it.
"""

from __future__ import annotations

import functools
import gc
import heapq
import json
import os
import selectors
import signal
import sys
import threading
import time
from collections import Counter
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from typing import Any

import pytest

from forktest.events import CollectError, Done, Event, Miss, Request, Selection, TestResult, Verified
from forktest.fingerprint import FILE, MODULE, Fingerprints, normalize_qualname
from forktest.selection import ImportGraph, Reason, changed_paths, config_files, select
from forktest.store import Dep, GitBaseline, Record, Store, StoreView
from forktest.worker import ProjectFiles, Recorder, run_items, worker_main

Emit = Callable[[Event], None]


class Cancelled(Exception):
    """The run was stopped from outside before every worker finished."""


class ClientGone(Exception):
    """The consumer of the event stream disconnected."""


class ZygotePlugin:
    def __init__(self, loop: Callable[[Runner], None]) -> None:
        self.loop = loop
        self.collect_errors: list[CollectError] = []
        self.collect_skips: list[str] = []

    @pytest.hookimpl(trylast=True)
    def pytest_configure(self, config: pytest.Config) -> None:
        reporter = config.pluginmanager.getplugin("terminalreporter")
        if reporter is not None:
            config.pluginmanager.unregister(reporter)

    def pytest_collectreport(self, report: pytest.CollectReport) -> None:
        if report.failed:
            self.collect_errors.append(CollectError(report.nodeid, str(report.longrepr)))
        elif report.skipped:
            self.collect_skips.append(report.nodeid)

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_makereport(self, item: pytest.Item, call: pytest.CallInfo[None]) -> Any:
        report = yield
        if call.excinfo is not None:
            report.forktest_trace = [(str(e.path), e.lineno + 1, e.name) for e in call.excinfo.traceback]
        return report

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtestloop(self, session: pytest.Session) -> bool:
        runner = Runner(session, self.collect_errors, self.collect_skips)
        try:
            self.loop(runner)
        finally:
            runner.close()
        return True


def schedule(
    items: Sequence[pytest.Item], duration_of: Callable[[str], float], failed: set[str], workers: int
) -> list[list[pytest.Item]]:
    """Split items into per-worker lists: module batches, failed-first, longest-processing-time.

    A batch that costs more than an even share of the run is split, by class
    first and then into consecutive runs, so module- and class-scoped
    fixtures are rebuilt as few times as the share allows.
    """

    def cost(batch: list[pytest.Item]) -> float:
        return sum(duration_of(i.nodeid) for i in batch)

    share = cost(list(items)) / max(workers, 1)
    batches: list[list[pytest.Item]] = []
    for module in _group(items, lambda nodeid: nodeid.split("::")[0]):
        if cost(module) <= share:
            batches.append(module)
            continue
        for group in _group(module, lambda nodeid: nodeid.rsplit("::", 1)[0]):
            batches.extend(_chunks(group, duration_of, share))
    batches.sort(key=lambda b: (not any(i.nodeid in failed for i in b), -cost(b)))
    n = min(workers, len(batches))
    assigned: list[list[pytest.Item]] = [[] for _ in range(n)]
    loads = [(0.0, k) for k in range(n)]
    for batch in batches:
        load, k = heapq.heappop(loads)
        assigned[k].extend(batch)
        heapq.heappush(loads, (load + cost(batch), k))
    return [a for a in assigned if a]


def _group(items: Iterable[pytest.Item], key: Callable[[str], str]) -> list[list[pytest.Item]]:
    groups: dict[str, list[pytest.Item]] = {}
    for item in items:
        groups.setdefault(key(item.nodeid), []).append(item)
    return list(groups.values())


def _chunks(items: list[pytest.Item], duration_of: Callable[[str], float], share: float) -> list[list[pytest.Item]]:
    out: list[list[pytest.Item]] = [[]]
    load = 0.0
    for item in items:
        d = duration_of(item.nodeid)
        if out[-1] and load + d > share:
            out.append([])
            load = 0.0
        out[-1].append(item)
        load += d
    return out


@dataclass
class _Worker:
    pid: int
    items: list[pytest.Item]
    pending: set[str]
    buf: bytes = b""
    done: bool = False
    crash: str | None = None


class Runner:
    """Selection, scheduling, forking and recording for one collected session."""

    def __init__(self, session: pytest.Session, collect_errors: list[CollectError], collect_skips: list[str]) -> None:
        self.session = session
        self.config = session.config
        self.items: list[pytest.Item] = list(session.items)
        self.collect_errors = collect_errors
        self.collect_skips = collect_skips
        self.root = os.path.realpath(str(self.config.rootpath))
        self.project = ProjectFiles(self.root)
        self.store = Store(os.path.join(self.root, ".forktest", "store.db"))
        self.fps = Fingerprints(self.root, self.store.file_infos())
        self.recorder = Recorder(self.project)
        self.recorder.install_audit_hook()
        self.worker_pids: set[int] = set()
        self._fixture_locs: dict[str, frozenset[tuple[str, str]]] = {}
        self._all_fixture_locs: frozenset[tuple[str, str]] | None = None

    def close(self) -> None:
        self.store.close()

    def graph(self) -> ImportGraph:
        known = {}
        for name, mod in list(sys.modules.items()):
            rel = self.project.rel(getattr(mod, "__file__", None) or "")
            if rel is not None:
                known[name] = rel
        return ImportGraph(self.root, self.fps, known)

    def imported_files(self) -> list[str]:
        return sorted(set(self.graph().known.values()))

    def third_party_modules(self) -> list[str]:
        out = []
        for name, mod in list(sys.modules.items()):
            filename = getattr(mod, "__file__", None)
            if not filename or name == "__main__" or name.split(".")[0] == "forktest":
                continue
            if self.project.rel(filename) is None:
                out.append(name)
        return sorted(out)

    def fixture_locs(self, item: pytest.Item) -> frozenset[tuple[str, str]]:
        if item.nodeid in self._fixture_locs:
            return self._fixture_locs[item.nodeid]
        out: set[tuple[str, str]] = set()
        info = getattr(item, "_fixtureinfo", None)
        for defs in (info.name2fixturedefs.values() if info else ()):
            for fixturedef in defs:
                func = getattr(fixturedef.func, "__func__", fixturedef.func)
                while hasattr(func, "__wrapped__"):
                    func = func.__wrapped__
                code = getattr(func, "__code__", None)
                rel = self.project.rel(code.co_filename) if code else None
                if rel is not None:
                    out.add((rel, code.co_qualname))
        self._fixture_locs[item.nodeid] = frozenset(out)
        return self._fixture_locs[item.nodeid]

    def all_fixture_locs(self) -> frozenset[tuple[str, str]]:
        if self._all_fixture_locs is None:
            self._all_fixture_locs = frozenset().union(*(self.fixture_locs(i) for i in self.items))
        return self._all_fixture_locs

    def run(
        self,
        req: Request,
        emit: Emit,
        wake_fd: int | None = None,
        should_cancel: Callable[[], bool] = lambda: True,
    ) -> None:
        """Run one request. When ``wake_fd`` turns readable, ``should_cancel`` decides whether to stop."""
        try:
            self._run(req, emit, wake_fd, should_cancel)
        finally:
            self.store.save_file_infos(self.fps.fresh)
            self.fps.fresh = {}

    def _run(
        self,
        req: Request,
        emit: Emit,
        wake_fd: int | None,
        should_cancel: Callable[[], bool],
    ) -> None:
        t0 = time.perf_counter()
        for error in self.collect_errors:
            emit(error)
        if self.collect_errors and not self.config.getoption("continue_on_collection_errors"):
            emit(Done(errors=len(self.collect_errors), duration_ms=_ms(t0)))
            return
        view = self.store.view()
        graph = self.graph()
        nodeids = [i.nodeid for i in self.items]
        current = functools.cache(self.fps.of)
        if req.mode == "all":
            reasons = {n: Reason.ALL for n in nodeids}
            changed: list[str] = []
        else:
            if req.mode == "since":
                baseline = GitBaseline(self.root, req.since or "HEAD")
                try:
                    view = view.rebased(baseline.hash_of)
                finally:
                    baseline.close()
            reasons = select(nodeids, view, current, graph, fixtures=self.all_fixture_locs())
            changed = changed_paths(nodeids, view, current)
        selected = [i for i in self.items if i.nodeid in reasons]
        counts = {str(r): n for r, n in sorted(Counter(reasons.values()).items())}
        emit(Selection(changed, len(selected), len(nodeids), counts, _ms(t0)))

        tally: Counter[str] = Counter()

        def on_result(result: TestResult) -> None:
            tally[result.outcome] += 1
            emit(result)

        cancel = (wake_fd, should_cancel)
        self.execute(selected, view, graph, req, on_result, cancel, nice=False, current=current)
        emit(
            Done(
                passed=tally["passed"],
                failed=tally["failed"],
                errors=tally["error"],
                # pytest counts a module that skips itself at import; only a full run is comparable.
                skipped=tally["skipped"] + (len(self.collect_skips) if req.mode == "all" else 0),
                xfailed=tally["xfailed"],
                xpassed=tally["xpassed"],
                duration_ms=_ms(t0),
            )
        )
        if not req.verify:
            return
        t1 = time.perf_counter()
        rest = [i for i in self.items if i.nodeid not in reasons]
        misses = 0

        def on_unselected(result: TestResult) -> None:
            nonlocal misses
            if result.outcome in ("failed", "error"):
                misses += 1
                emit(Miss(result.nodeid, result.message or ""))

        self.execute(rest, view, graph, req, on_unselected, cancel, nice=req.nice, current=current)
        emit(Verified(len(rest), misses, _ms(t1)))

    def execute(
        self,
        items: list[pytest.Item],
        view: StoreView,
        graph: ImportGraph,
        req: Request,
        on_result: Callable[[TestResult], None],
        cancel: tuple[int | None, Callable[[], bool]],
        nice: bool,
        current: Callable[[str], dict[str, str] | None],
    ) -> None:
        if not items:
            return
        records: list[Record] = []
        config_deps = [Dep(p, FILE, current(p)[FILE]) for p in config_files(self.root) if current(p)]
        maxfail = self.config.getoption("maxfail") or 0
        stopped = False
        failures = 0

        def handle(msg: dict[str, Any], worker: _Worker | None) -> None:
            nonlocal failures, stopped
            if msg["type"] == "result":
                result = TestResult(**msg["result"])
                item = self._item(result.nodeid)
                if worker is not None:
                    worker.pending.discard(result.nodeid)
                deps = self.deps_for(item, msg["hits"], msg["reads"], graph, config_deps, current)
                records.append(Record(result.nodeid, result.outcome, result.duration, deps))
                on_result(result)
                if result.outcome in ("failed", "error"):
                    failures += 1
                    if maxfail and failures >= maxfail and not stopped:
                        stopped = True
                        self._kill_workers()
            elif msg["type"] == "done" and worker is not None:
                worker.done = True
            elif msg["type"] == "crash" and worker is not None:
                worker.crash = msg["message"]

        try:
            if req.strict or self._threads_block_fork():
                self.recorder.start()
                try:
                    run_items(items, lambda msg: handle(msg, None), self.recorder, self.project)
                finally:
                    self.recorder.stop()
            else:
                workers = req.workers or os.cpu_count() or 2
                failed = {n for n, row in view.tests.items() if row.outcome in ("failed", "error")}
                batches = schedule(items, lambda n: view.tests[n].duration if n in view.tests else 0.01, failed, workers)
                self._run_forked(batches, nice, handle, cancel, lambda: stopped, on_result)
        finally:
            self.store.record(records)
            self.store.meta_set("preimport", json.dumps(self.third_party_modules()))

    def _threads_block_fork(self) -> bool:
        extra = threading.active_count() - 1
        if extra > 0:
            sys.stderr.write(f"forktest: {extra} extra thread(s) alive in the zygote; running in-process instead of forking\n")
        return extra > 0

    def _item(self, nodeid: str) -> pytest.Item:
        if not hasattr(self, "_by_nodeid"):
            self._by_nodeid = {i.nodeid: i for i in self.items}
        return self._by_nodeid[nodeid]

    def _reinit_capture(self) -> None:
        capman = self.config.pluginmanager.getplugin("capturemanager")
        if capman is not None and capman.is_globally_capturing():
            capman.stop_global_capturing()
            capman.start_global_capturing()
            capman.suspend_global_capture()

    def _kill_workers(self) -> None:
        for pid in list(self.worker_pids):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def _run_forked(
        self,
        batches: list[list[pytest.Item]],
        nice: bool,
        handle: Callable[[dict[str, Any], _Worker], None],
        cancel: tuple[int | None, Callable[[], bool]],
        stopped: Callable[[], bool],
        on_result: Callable[[TestResult], None],
    ) -> None:
        wake_fd, should_cancel = cancel
        sel = selectors.DefaultSelector()
        workers: dict[int, _Worker] = {}
        gc.freeze()
        try:
            for batch in batches:
                r, w = os.pipe()
                pid = os.fork()
                if pid == 0:
                    os.close(r)
                    for other in workers:
                        os.close(other)
                    worker_main(batch, w, self.recorder, self.project, self._reinit_capture, nice)
                os.close(w)
                workers[r] = _Worker(pid, batch, {i.nodeid for i in batch})
                self.worker_pids.add(pid)
                sel.register(r, selectors.EVENT_READ)
        finally:
            gc.unfreeze()
        if wake_fd is not None:
            sel.register(wake_fd, selectors.EVENT_READ)
        try:
            while workers:
                for key, _ in sel.select():
                    if key.fd == wake_fd:
                        if should_cancel():
                            raise Cancelled
                        continue
                    worker = workers[key.fd]
                    data = os.read(key.fd, 1 << 16)
                    if data:
                        worker.buf += data
                        *lines, worker.buf = worker.buf.split(b"\n")
                        for line in lines:
                            handle(json.loads(line), worker)
                        continue
                    sel.unregister(key.fd)
                    os.close(key.fd)
                    del workers[key.fd]
                    _, status = os.waitpid(worker.pid, 0)
                    self.worker_pids.discard(worker.pid)
                    if worker.pending and not stopped():
                        why = worker.crash or f"worker exited before running this test (wait status {status})"
                        for item in worker.items:
                            if item.nodeid in worker.pending:
                                on_result(TestResult(item.nodeid, "error", 0.0, message=why))
        finally:
            sel.close()
            self._kill_workers()
            for fd, worker in workers.items():
                os.close(fd)
                os.waitpid(worker.pid, 0)
                self.worker_pids.discard(worker.pid)

    def deps_for(
        self,
        item: pytest.Item,
        hits: Iterable[Sequence[str]],
        reads: Iterable[str],
        graph: ImportGraph,
        config_deps: list[Dep],
        current: Callable[[str], dict[str, str] | None],
    ) -> list[Dep]:
        deps: set[Dep] = set(config_deps)

        def add_function(rel: str, qualname: str) -> None:
            fp = current(rel)
            if fp is not None:
                q = normalize_qualname(qualname, fp)
                deps.add(Dep(rel, q, fp[q]))

        for filename, qualname in hits:
            rel = self.project.rel(filename)
            if rel is not None:
                add_function(rel, qualname)
        for rel, qualname in self.fixture_locs(item):
            add_function(rel, qualname)
        for path in reads:
            rel = self.project.rel(path)
            fp = current(rel) if rel is not None else None
            if rel is not None and fp is not None:
                deps.add(Dep(rel, FILE, fp[FILE]))
        for path in graph.closure(item.nodeid.split("::")[0]):
            fp = current(path)
            if fp is not None:
                key = MODULE if MODULE in fp else FILE
                deps.add(Dep(path, key, fp[key]))
        return sorted(deps)


def _ms(t0: float) -> int:
    return int((time.perf_counter() - t0) * 1000)


def run_oneshot(request: Request, pytest_args: list[str], emit: Emit) -> int:
    """Collect in this process, run one request, and return pytest's exit code."""
    return int(pytest.main(pytest_args, plugins=[ZygotePlugin(lambda runner: runner.run(request, emit))]))
