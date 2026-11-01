"""The Python side of forktest: L0, L1 and workers. Unit U2. See RUST_PLAN.md for the protocol.

Every module imported here is one pytest imports anyway, so nothing a test could lean on leaks into it.
"""

import array
import gc
import json
import os
import signal
import socket
import struct
import sys
import threading
import time

# dyld has loaded forktest's time zone fix by now. Tests and the processes they start should not see it.
if "forktest_tzfix" in os.environ.get("DYLD_INSERT_LIBRARIES", ""):
    kept = [p for p in os.environ["DYLD_INSERT_LIBRARIES"].split(":") if p and "forktest_tzfix" not in p]
    if kept:
        os.environ["DYLD_INSERT_LIBRARIES"] = ":".join(kept)
    else:
        del os.environ["DYLD_INSERT_LIBRARIES"]

if sys.path and sys.path[0] == os.path.dirname(os.path.abspath(__file__)):
    # `python shim.py` puts the shim's directory first; the `pytest` console script would not.
    del sys.path[0]

import pytest

SOCK = os.environ.get("FORKTEST_SOCK", "")
FORK_SAFE_POOLS = {"numpy", "scipy"}
EXCLUDED_DIRS = frozenset(
    {
        ".git", ".hg", ".venv", "venv", ".tox", ".nox", "__pycache__", ".forktest", ".pytest_cache",
        ".hypothesis", ".mypy_cache", ".ruff_cache", "node_modules", "site-packages",
    }
)


class Conn:
    def __init__(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(SOCK)
        self.rfile = self.sock.makefile("rb")

    def send(self, msg, blob=b""):
        data = json.dumps(msg, ensure_ascii=False).encode("utf-8", "replace")
        self.sock.sendall(struct.pack(">II", 4 + len(data) + len(blob), len(data)) + data + blob)

    def recv(self):
        head = self.rfile.read(4)
        if len(head) < 4:
            return None
        (total,) = struct.unpack(">I", head)
        body = self.rfile.read(total)
        if len(body) < total:
            return None
        (json_len,) = struct.unpack_from(">I", body)
        return json.loads(body[4 : 4 + json_len])

    def close(self):
        self.rfile.close()
        self.sock.close()


def os_threads():
    if sys.platform == "linux":
        return len(os.listdir("/proc/self/task"))
    if sys.platform == "darwin":
        return _darwin_threads()
    return None


_proc_pidinfo = None


def _darwin_threads():
    global _proc_pidinfo
    import ctypes

    if _proc_pidinfo is None:
        _proc_pidinfo = ctypes.CDLL(None).proc_pidinfo
        _proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
        _proc_pidinfo.restype = ctypes.c_int
    buf = ctypes.create_string_buffer(96)
    if _proc_pidinfo(os.getpid(), 4, 0, buf, 96) != 96:  # PROC_PIDTASKINFO, sizeof(struct proc_taskinfo)
        return None
    return struct.unpack_from("=i", buf.raw, 84)[0]  # pti_threadnum


def flush_and_exit(code):
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.flush()
        except Exception:
            pass
    os._exit(code)


def module_file(mod):
    try:
        filename = getattr(mod, "__file__", None)
    except Exception:  # lazy module proxies can raise on any attribute
        return None
    return filename if isinstance(filename, str) else None


def default_signals():
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    signal.signal(signal.SIGINT, signal.SIG_DFL)


class ProjectFiles:
    """Decides whether an absolute path is project code, with a per-path cache."""

    def __init__(self, root):
        self.root = os.path.realpath(root).rstrip("/") + "/"
        own = os.path.dirname(os.path.realpath(__file__))
        self.excluded_prefixes = tuple(
            p.rstrip("/") + "/" for p in {sys.prefix, sys.base_prefix, sys.exec_prefix, own}
        )
        self._cache = {}

    def rel(self, path):
        try:
            return self._cache[path]
        except KeyError:
            pass
        rel = None
        if isinstance(path, str) and os.path.isabs(path):
            real = os.path.realpath(path)
            if real.startswith(self.root) and not real.startswith(self.excluded_prefixes):
                candidate = real[len(self.root) :]
                if not candidate.startswith("..") and not EXCLUDED_DIRS.intersection(candidate.split("/")):
                    rel = candidate
        self._cache[path] = rel
        return rel


class Recorder:
    """Project functions that ran and project files read between ``begin`` and ``end``.

    ``PY_START`` fires once per code object between ``restart_events`` calls because the callback
    returns ``DISABLE``, so steady-state overhead is one callback per distinct function per test.
    """

    def __init__(self, project):
        self.project = project
        self.hits = set()
        self.reads = set()
        self.tool_id = None
        self.recording = False

    def start(self):
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
        sys.addaudithook(self._on_open)

    def begin(self):
        self.hits = set()
        self.reads = set()
        self.recording = True
        sys.monitoring.restart_events()

    def end(self):
        self.recording = False
        hits, reads = self.hits, self.reads
        self.hits, self.reads = set(), set()
        return hits, reads

    def _on_start(self, code, offset):
        if self.recording:
            rel = self.project.rel(code.co_filename)
            if rel is not None:
                self.hits.add((rel, code.co_qualname))
        return sys.monitoring.DISABLE

    def _on_open(self, event, args):
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
        rel = self.project.rel(path)
        if rel is not None:
            self.reads.add(rel)


class Restriction:
    """Collects only ``files``: directories with no target beneath them and other test files are ignored."""

    def __init__(self, root, files):
        self.targets = {os.path.realpath(os.path.join(root, f)) for f in files}
        self.dirs = set()
        for target in self.targets:
            d = os.path.dirname(target)
            while d not in self.dirs:
                self.dirs.add(d)
                parent = os.path.dirname(d)
                if parent == d:
                    break
                d = parent

    def ignores(self, path):
        real = os.path.realpath(path)
        if os.path.isdir(real):
            return real not in self.dirs
        return real not in self.targets and os.path.basename(real) not in ("__init__.py", "conftest.py")


class Narrowing:
    """In the modules and classes on the paths of the ``only`` nodeids, makes items for those tests' names alone.

    Every parametrization of a named function is made: its ids only exist once the function is collected.
    """

    def __init__(self, only):
        self.allowed = {}
        for nodeid in only:
            path, _, rest = nodeid.partition("::")
            parts = [path, *rest.partition("[")[0].split("::")]
            for i in range(1, len(parts)):
                self.allowed.setdefault("::".join(parts[:i]), set()).add(parts[i])

    def drops(self, collector, name):
        allowed = self.allowed.get(collector.nodeid)
        return allowed is not None and name not in allowed


class Sender:
    """Sends results with each ``(path, qualname)`` named once per connection and hits as packed ids."""

    def __init__(self, conn):
        self.conn = conn
        self.ids = {}

    def result(self, result, cost, hits, reads, replay=None):
        new = []
        ids = array.array("I")
        for name in hits:
            i = self.ids.get(name)
            if i is None:
                i = self.ids[name] = len(self.ids)
                new.append(name)
            ids.append(i)
        if sys.byteorder != "little":
            ids.byteswap()
        msg = {"type": "result", **result, "cost": cost, "names": new, "reads": sorted(reads)}
        if replay is not None:
            msg["pytest"] = replay
        self.conn.send(msg, ids.tobytes())


class _ReportCollector:
    """One item's reports, and with ``replay`` what pytest's reporter needs of them and of its warnings."""

    def __init__(self, config, replay):
        self.config = config
        self.replay = replay
        self.reports = []
        self.warnings = []
        self.running = False

    def begin(self):
        self.reports = []
        self.warnings = []
        self.running = True

    def end(self):
        self.running = False
        if not self.replay:
            return None
        return {"reports": [serialized(self.config, r) for r in self.reports], "warnings": self.warnings}

    def pytest_runtest_logreport(self, report):
        self.reports.append(report)

    def pytest_warning_recorded(self, warning_message, when, nodeid, location):
        # The hook is historic: registering this replays every warning recorded before, which `running` keeps out.
        if self.running and self.replay and when == "runtest":
            self.warnings.append(warning_data(warning_message, when, nodeid, location))


def serialized(config, report):
    data = config.hook.pytest_report_to_serializable(config=config, report=report)
    data.pop("forktest_trace", None)
    if hasattr(report.longrepr, "toterminal") and isinstance(data["longrepr"], str):
        # Serialized as its text; the reporter restores a repr, since pytest tells a plain string apart from one.
        data["forktest_repr"] = type(report.longrepr).__name__
    return data


def warning_data(warning_message, when, nodeid, location):
    category = warning_message.category
    return {
        "message": str(warning_message.message),
        "category": [category.__module__, category.__qualname__],
        "filename": warning_message.filename,
        "lineno": warning_message.lineno,
        "line": warning_message.line,
        "when": when,
        "nodeid": nodeid,
        "location": location,
    }


def result_of(nodeid, reports, project):
    duration = sum(r.duration for r in reports)
    out = {"nodeid": nodeid, "duration": duration, "file": None, "line": None, "message": None, "trace": []}
    failed = [r for r in reports if r.failed]
    if failed:
        first = failed[0]
        message, out["file"], out["line"], out["trace"] = failure_details(first, project)
        for extra in failed[1:]:
            message += f"\n--- {extra.when} ---\n" + failure_details(extra, project)[0]
        return {**out, "outcome": "failed" if first.when == "call" else "error", "message": message}
    skipped = [r for r in reports if r.skipped]
    if skipped and hasattr(skipped[0], "wasxfail"):
        return {**out, "outcome": "xfailed", "message": skipped[0].wasxfail or "xfail"}
    if skipped:
        lr = skipped[0].longrepr
        return {**out, "outcome": "skipped", "message": lr[2] if isinstance(lr, tuple) else str(lr)}
    if any(hasattr(r, "wasxfail") for r in reports):
        return {**out, "outcome": "xpassed"}
    return {**out, "outcome": "passed"}


def failure_details(report, project):
    lr = report.longrepr
    crash = getattr(lr, "reprcrash", None)
    message = crash.message if crash is not None else str(lr)
    if message.startswith("AssertionError: assert "):
        message = message[len("AssertionError: ") :]
    trace = []
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


class Fixtures:
    """Static fixture locations: parametrized items share one fixture info, and infos share definitions."""

    def __init__(self, project):
        self.project = project
        self._by_info = {}

    def _loc(self, fixturedef):
        try:
            func = getattr(fixturedef.func, "__func__", fixturedef.func)
            while hasattr(func, "__wrapped__"):
                func = func.__wrapped__
            code = getattr(func, "__code__", None)
        except Exception:
            return None
        rel = self.project.rel(code.co_filename) if code is not None else None
        return None if rel is None else (rel, code.co_qualname)

    def of(self, item):
        info = getattr(item, "_fixtureinfo", None)
        if info is None:
            return frozenset()
        locs = self._by_info.get(id(info))
        if locs is None:
            defs = (d for ds in info.name2fixturedefs.values() for d in ds)
            locs = self._by_info[id(info)] = (info, frozenset(loc for d in defs if (loc := self._loc(d))))
        return locs[1]

    def of_all(self, items):
        out = set()
        for item in items:
            out |= self.of(item)
        return sorted(out)


def run_items(items, sender, recorder, project, fixtures, replay):
    if not items:
        return
    collector = _ReportCollector(items[0].config, replay)
    pluginmanager = items[0].config.pluginmanager
    pluginmanager.register(collector)
    try:
        for i, item in enumerate(items):
            nextitem = items[i + 1] if i + 1 < len(items) else None
            collector.begin()
            started = time.perf_counter()
            recorder.begin()
            # The full hook, not runtestprotocol: plugins such as warnings (filterwarnings marks) wrap it.
            item.ihook.pytest_runtest_protocol(item=item, nextitem=nextitem)
            hits, reads = recorder.end()
            pytest_side = collector.end()
            result = result_of(item.nodeid, collector.reports, project)
            # Building a failure's report can cost 100 times the test, and the reports' durations leave it out.
            cost = time.perf_counter() - started
            sender.result(result, cost, hits | fixtures.of(item), reads, pytest_side)
    finally:
        pluginmanager.unregister(collector)


def rewrite_names():
    for finder in sys.meta_path:
        names = getattr(finder, "_must_rewrite", None)
        if isinstance(names, set):
            return names
    return set()


class BasePlugin:
    def __init__(self, conn, root, files, only=None):
        self.conn = conn
        self.root = root
        self.project = ProjectFiles(root)
        self.recorder = Recorder(self.project)
        self.fixtures = Fixtures(self.project)
        self.restriction = Restriction(root, files) if files is not None else None
        self.narrowing = Narrowing(only) if only else None
        self.errors = []
        self.skips = []
        # What pytest's reporter shows of collection: failed and skipped collect reports, and collection warnings.
        self.collect_reports = []
        self.collect_warnings = []
        self.deselected = 0
        self.items = []
        self.config = None

    def pytest_addoption(self, parser):
        parser.addini("forktest_fork_from", "Where forktest workers fork from: auto, collected, imported or spawn.")

    @pytest.hookimpl(trylast=True)
    def pytest_configure(self, config):
        self.config = config
        reporter = config.pluginmanager.getplugin("terminalreporter")
        if reporter is not None:
            config.pluginmanager.unregister(reporter)
        self.recorder.start()

    @pytest.hookimpl(tryfirst=True)
    def pytest_ignore_collect(self, collection_path, config):
        if self.restriction is not None and self.restriction.ignores(str(collection_path)):
            return True
        return None

    @pytest.hookimpl(tryfirst=True)
    def pytest_pycollect_makeitem(self, collector, name, obj):
        if self.narrowing is not None and self.narrowing.drops(collector, name):
            return []
        return None

    def pytest_collectreport(self, report):
        if report.failed:
            self.errors.append([report.nodeid, str(report.longrepr)])
        elif report.skipped:
            self.skips.append(report.nodeid)
        if not report.passed:
            self.collect_reports.append(serialized(self.config, report))

    def pytest_warning_recorded(self, warning_message, when, nodeid, location):
        if when == "collect":
            self.collect_warnings.append(warning_data(warning_message, when, nodeid, location))

    def pytest_deselected(self, items):
        self.deselected += len(items)

    @pytest.hookimpl(trylast=True)
    def pytest_collection_modifyitems(self, session, config, items):
        self.items = items

    @pytest.hookimpl(wrapper=True)
    def pytest_runtest_makereport(self, item, call):
        report = yield
        if call.excinfo is not None:
            report.forktest_trace = [(str(e.path), e.lineno + 1, e.name) for e in call.excinfo.traceback]
        return report

    def reinit_capture(self):
        capman = self.config.pluginmanager.getplugin("capturemanager")
        if capman is not None and capman.is_globally_capturing():
            capman.stop_global_capturing()
            capman.start_global_capturing()
            capman.suspend_global_capture()


class L1Plugin(BasePlugin):
    """Collects, reports what it found, then serves runs from the collected session."""

    def __init__(self, conn, root, files, only):
        super().__init__(conn, root, files, only)
        self.per_file = {}
        self.served = False

    @pytest.hookimpl(wrapper=True)
    def pytest_make_collect_report(self, collector):
        if not isinstance(collector, (pytest.Module, pytest.Class)):
            return (yield)
        rel = self.project.rel(str(collector.path))
        self.recorder.begin()
        try:
            return (yield)
        finally:
            hits, reads = self.recorder.end()
            if rel is not None:
                names, files = self.per_file.setdefault(rel, (set(), set()))
                names |= hits
                files |= reads

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtestloop(self, session):
        self.served = True
        self.conn.send(self.collected(session))
        self.serve(session)
        return True

    def collected(self, session):
        items = []
        for item in self.items:
            items.append({"nodeid": item.nodeid, "path": self.project.rel(str(item.path)) or str(item.path)})
        return {
            "type": "collected",
            "items": items,
            "errors": self.errors,
            "skips": self.skips,
            "maxfail": self.config.getoption("maxfail") or 0,
            "rootdir": str(self.config.rootpath),
            "python_files": self.config.getini("python_files"),
            "fork_from": self.config.getini("forktest_fork_from") or None,
            "modules": self.known_modules(),
            "third_party": self.third_party_modules(),
            "rewrite": sorted(rewrite_names()),
            "fixtures": self.fixtures.of_all(self.items),
            "per_file": {
                path: {"names": sorted(names), "reads": sorted(reads)} for path, (names, reads) in self.per_file.items()
            },
            "py_threads": threading.active_count(),
            "pytest": {"collect_reports": self.collect_reports, "warnings": self.collect_warnings, "deselected": self.deselected},
        }

    def known_modules(self):
        known = {}
        for name, mod in list(sys.modules.items()):
            rel = self.project.rel(module_file(mod) or "")
            if rel is not None:
                known[name] = rel
        return known

    def third_party_modules(self):
        rewrite = rewrite_names()
        out = []
        for name, mod in list(sys.modules.items()):
            filename = module_file(mod)
            if not filename or name in ("__main__", "__mp_main__") or self.project.rel(filename) is not None:
                continue
            parts = name.split(".")
            if any(".".join(parts[: i + 1]) in rewrite for i in range(len(parts))):
                continue
            out.append(name)
        return sorted(out)

    def serve(self, session):
        by_id = {item.nodeid: item for item in self.items}
        while True:
            msg = self.conn.recv()
            if msg is None or msg["type"] == "exit":
                os.killpg(0, signal.SIGKILL)
            if msg["type"] == "run":
                batches = [[by_id[n] for n in batch if n in by_id] for batch in msg["batches"]]
                self.fork_workers(msg["rid"], batches, msg["nice"], msg["replay"])
            elif msg["type"] == "run_in_process":
                sender = Sender(self.conn)
                items = [by_id[n] for n in msg["nodeids"] if n in by_id]
                run_items(items, sender, self.recorder, self.project, self.fixtures, msg["replay"])
                self.conn.send({"type": "done", "rid": msg["rid"]})

    def fork_workers(self, rid, batches, nice, replay):
        pids = {}
        gc.freeze()
        try:
            for index, items in enumerate(batches):
                pid = os.fork()
                if pid == 0:
                    self.forked_worker(rid, index, items, nice, replay)
                pids[pid] = index
        finally:
            gc.unfreeze()
        wait_and_report(self.conn, rid, pids)

    def forked_worker(self, rid, index, items, nice, replay):
        try:
            default_signals()
            self.conn.close()
            conn = Conn()
            conn.send({"type": "hello", "role": "worker", "pid": os.getpid(), "rid": rid, "index": index})
            try:
                if nice:
                    os.nice(10)
                self.reinit_capture()
                run_items(items, Sender(conn), self.recorder, self.project, self.fixtures, replay)
                conn.send({"type": "done"})
            except BaseException as exc:
                conn.send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        finally:
            flush_and_exit(0)


class BatchPlugin(BasePlugin):
    """A worker that collects its batch's files itself and runs exactly its nodeids, in order."""

    def __init__(self, conn, root, files, nodeids, replay):
        super().__init__(conn, root, files)
        self.nodeids = nodeids
        self.replay = replay

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtestloop(self, session):
        started = {"type": "started", "maxfail": self.config.getoption("maxfail") or 0}
        if self.replay:
            started["pytest"] = {"warnings": self.collect_warnings}
        self.conn.send(started)
        failed = (r for r in self.collect_reports if r["outcome"] == "failed")
        for (nodeid, message), report in zip(self.errors, failed):
            msg = {"type": "collect_error", "nodeid": nodeid, "message": message}
            if self.replay:
                msg["report"] = report
            self.conn.send(msg)
        by_id = {item.nodeid: item for item in self.items}
        items = [by_id[n] for n in self.nodeids if n in by_id]
        try:
            run_items(items, Sender(self.conn), self.recorder, self.project, self.fixtures, self.replay)
            self.conn.send({"type": "done"})
        except BaseException as exc:
            self.conn.send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        flush_and_exit(0)


def wait_and_report(conn, rid, pids):
    while pids:
        pid, status = os.waitpid(-1, 0)
        index = pids.pop(pid, None)
        if index is not None:
            conn.send({"type": "exited", "rid": rid, "index": index, "status": status})


def batch_worker(conn, root, args, nodeids, files, nice, replay):
    """Runs one batch with its own collection, for the Imported and Spawn fork points. Never returns."""
    try:
        if nice:
            os.nice(10)
        rc = pytest.main(list(args), plugins=[BatchPlugin(conn, root, files, nodeids, replay)])
        conn.send({"type": "crash", "message": f"pytest exited with code {int(rc)} before running the batch"})
    except BaseException as exc:
        try:
            conn.send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        except Exception:
            pass
    flush_and_exit(0)


def l1_main(conn, root, msg):
    """Collects ``msg["files"]`` (everything when null) narrowed to ``msg["only"]``, then serves runs. Never returns."""
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    plugin = L1Plugin(conn, root, msg["files"], msg.get("only"))
    try:
        rc = pytest.main(list(msg["args"]), plugins=[plugin])
        if not plugin.served:
            conn.send({"type": "failed", "code": int(rc)})
    except BaseException as exc:
        try:
            conn.send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        except Exception:
            pass
    flush_and_exit(0)


class Placeholder(pytest.Item):
    """A test the reporter shows: it ran in a worker, so here it only has a nodeid."""

    def runtest(self):
        pass


def warning_message_of(data):
    import importlib
    import warnings

    module, qualname = data["category"]
    try:
        category = importlib.import_module(module)
        for part in qualname.split("."):
            category = getattr(category, part)
    except Exception:
        category = type(qualname.rpartition(".")[2], (UserWarning,), {"__module__": module})
    return warnings.WarningMessage(data["message"], category, data["filename"], data["lineno"], line=data["line"])


class Replay:
    """Turns pytest's session into a reporter: Rust sends the run's reports, in collection order, and this replays
    them through the hooks pytest's terminal reporter, junitxml and other plugins listen to. Nothing is collected or
    run here.

    A reporter can be started before its run: it parses the args and loads plugins and conftests, then waits for a
    run to hand it the terminal before pytest configures, so the terminal writer decides colour and width from the
    real terminal. stderr waits for ``begin``, so a config error L1 reports anyway is not printed twice, and Rust
    sends ``begin`` once collection is done, so nothing reaches the terminal before forktest's headline.
    """

    def __init__(self, conn):
        self.conn = conn
        self.stderr_fd = None
        self.begin = None

    def pytest_addoption(self, parser):
        parser.addini("forktest_fork_from", "Where forktest workers fork from: auto, collected, imported or spawn.")

    @pytest.hookimpl(wrapper=True, tryfirst=True)
    def pytest_load_initial_conftests(self, early_config):
        ns = early_config.known_args_namespace
        # fd capture would remember the stdout this process had before the handover and put it back at the end.
        # Nothing runs here, so capturing at the sys level is the same.
        if ns.capture == "fd":
            ns.capture = "sys"
        # pytest-cov would measure this process, which runs no tests, and report that as the run's coverage.
        if hasattr(ns, "no_cov"):
            ns.no_cov = True
        return (yield)

    @pytest.hookimpl(wrapper=True, tryfirst=True)
    def pytest_cmdline_main(self, config):
        _, fds, _, _ = socket.recv_fds(self.conn.sock, 1, 2)
        out, self.stderr_fd = fds
        os.dup2(out, 1)
        os.close(out)
        sys.stdout.reconfigure(line_buffering=os.isatty(1))
        for name, value in self.recv()["env"]:
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        return (yield)

    @pytest.hookimpl(wrapper=True, tryfirst=True)
    def pytest_sessionstart(self, session):
        from _pytest import timing

        self.begin = self.recv()
        os.dup2(self.stderr_fd, 2)
        if self.begin["headline"] is not None:
            sys.stdout.write(self.begin["headline"] + "\n")
            sys.stdout.flush()
        self.conn.send({"type": "began"})
        # The session's clocks (the summary's "in 0.52s", JUnit's suite time) start when forktest's run started.
        shift = max(0.0, time.time() - self.begin["started"])
        clocks = {name: getattr(timing, name) for name in ("time", "perf_counter") if hasattr(timing, name)}
        for name, clock in clocks.items():
            setattr(timing, name, lambda clock=clock: clock() - shift)
        try:
            return (yield)
        finally:
            for name, clock in clocks.items():
                setattr(timing, name, clock)

    @pytest.hookimpl(tryfirst=True)
    def pytest_collection(self, session):
        from _pytest.reports import CollectReport

        config = session.config
        hook = config.hook
        terminal = config.pluginmanager.getplugin("terminalreporter")
        if terminal is not None:
            terminal.pytest_collection()
        begin = self.begin
        for data in begin["warnings"]:
            self.warn(hook, data)
        for data in begin["collect_reports"]:
            hook.pytest_collectreport(report=report_of(config, data))
        items = [Placeholder.from_parent(session, name=n, nodeid=n) for n in begin["items"]]
        deselected = [Placeholder.from_parent(session, name=f"deselected{i}", nodeid=f"deselected{i}") for i in range(begin["deselected"])]
        hook.pytest_collectreport(report=CollectReport("", "passed", None, items + deselected))
        if deselected:
            hook.pytest_deselected(items=deselected)
        session.items = items
        session.testscollected = len(items)
        hook.pytest_collection_finish(session=session)
        return True

    @pytest.hookimpl(tryfirst=True)
    def pytest_runtestloop(self, session):
        if session.testsfailed and not session.config.option.continue_on_collection_errors:
            n = session.testsfailed
            raise session.Interrupted(f"{n} error{'s' if n != 1 else ''} during collection")
        config = session.config
        hook = config.hook
        while True:
            msg = self.recv()
            kind = msg["type"]
            if kind == "end":
                return True
            if kind == "collect":
                hook.pytest_collectreport(report=report_of(config, msg["report"]))
                continue
            if kind == "warnings":
                for data in msg["warnings"]:
                    self.warn(hook, data)
                continue
            if kind == "lost":
                self.replay_test(hook, [lost_report(msg["nodeid"], msg["message"])], [])
            else:
                reports = [report_of(config, d) for d in msg["reports"]]
                self.replay_test(hook, reports, msg["warnings"])
            if session.shouldfail:
                raise session.Failed(session.shouldfail)
            if session.shouldstop:
                raise session.Interrupted(session.shouldstop)

    def replay_test(self, hook, reports, warnings):
        first = reports[0]
        hook.pytest_runtest_logstart(nodeid=first.nodeid, location=first.location)
        for report in reports:
            hook.pytest_runtest_logreport(report=report)
        for data in warnings:
            self.warn(hook, data)
        hook.pytest_runtest_logfinish(nodeid=first.nodeid, location=first.location)

    def warn(self, hook, data):
        location = tuple(data["location"]) if data["location"] is not None else None
        kwargs = {"warning_message": warning_message_of(data), "when": data["when"], "nodeid": data["nodeid"], "location": location}
        hook.pytest_warning_recorded.call_historic(kwargs=kwargs)

    def recv(self):
        msg = self.conn.recv()
        if msg is None:
            flush_and_exit(1)
        return msg


def report_of(config, data):
    """The report a worker serialized. JSON has no tuples, and pytest tells a skip's ``longrepr`` by being one."""
    from _pytest.reports import CollectErrorRepr

    for key in ("longrepr", "location"):
        if isinstance(data.get(key), list):
            data[key] = tuple(data[key])
    kind = data.pop("forktest_repr", None)
    report = config.hook.pytest_report_from_serializable(config=config, data=data)
    if kind == "CollectErrorRepr":
        report.longrepr = CollectErrorRepr(report.longrepr)
    elif kind is not None:
        report.longrepr = TextRepr(report.longrepr)
    return report


class TextRepr:
    """A failure representation that only has its text, as pytest-xdist's workers send one."""

    def __init__(self, text):
        self.text = text

    def __str__(self):
        return self.text

    def toterminal(self, out):
        for line in self.text.split("\n"):
            out.line(line)


def lost_report(nodeid, message):
    from _pytest.reports import TestReport

    path, _, name = nodeid.partition("::")
    return TestReport(nodeid, (path, None, name), {}, "failed", message, "setup")


def reporter_main(conn, args):
    """pytest's own reporter for one run: replays what Rust forwards. Never returns."""
    # What it inherited is never garbage of its session; pytest's unconfigure collects garbage twice over it.
    gc.freeze()
    try:
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)
        os.close(devnull)
        rc = pytest.main(list(args), plugins=[Replay(conn)])
        for stream in (sys.stdout, sys.stderr):
            stream.flush()
        conn.send({"type": "exit", "code": int(rc)})
    except BaseException as exc:
        try:
            conn.send({"type": "crash", "message": f"{type(exc).__name__}: {exc}"})
        except Exception:
            pass
    flush_and_exit(0)


def preimport(names, avoid=()):
    """Imports one top-level package at a time; returns the tolerated thread count and the dropped packages.

    A package that starts threads is dropped, and so is one that imports a module in `avoid`: pytest imports its
    plugins itself to rewrite their asserts, and one imported earlier gets a warning a `-W error` suite fails on.
    """
    import importlib

    tops = {}
    for name in names:
        tops.setdefault(name.split(".")[0], []).append(name)
    tolerated = 0
    dropped = []
    before = os_threads() if tops else None
    avoid = set(avoid)
    present = avoid.intersection(sys.modules)
    for top, mods in tops.items():
        for name in mods:
            try:
                importlib.import_module(name)
            except BaseException:  # the list came from an earlier environment; a name that no longer imports is fine
                pass
        after = os_threads()
        now = avoid.intersection(sys.modules)
        if now - present:
            dropped.append(top)
            present = now
            before = after
            continue
        if before is None or after is None:
            continue
        if after > before:
            if top in FORK_SAFE_POOLS:
                tolerated += after - before
            else:
                dropped.append(top)
        before = after
    return tolerated, dropped, before


def l0_main():
    conn = Conn()
    conn.send({"type": "hello", "role": "l0", "pid": os.getpid(), "python": sys.version.split()[0]})
    config = conn.recv()
    if config is None:
        os._exit(0)
    root = config["root"]
    tolerated, dropped, threads = preimport(config["preimport"], config.get("avoid", ()))
    conn.send(
        {
            "type": "ready",
            "os_threads": threads,
            "py_threads": threading.active_count(),
            "tolerated_threads": tolerated,
            "dropped": dropped,
        }
    )
    l1_pids = []
    while True:
        try:
            while os.waitpid(-1, os.WNOHANG)[0]:
                pass
        except ChildProcessError:
            pass
        msg = conn.recv()
        if msg is None or msg["type"] == "exit":
            for pid in l1_pids:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except OSError:
                    pass
            os.killpg(0, signal.SIGKILL)
        if msg["type"] == "collect":
            gc.freeze()
            pid = os.fork()
            if pid == 0:
                try:
                    os.setpgid(0, 0)
                    conn.close()
                    l1 = Conn()
                    l1.send({"type": "hello", "role": "l1", "pid": os.getpid(), "cid": msg["cid"]})
                    l1_main(l1, root, msg)
                finally:
                    os._exit(1)
            gc.unfreeze()
            l1_pids.append(pid)
        elif msg["type"] == "reporter":
            pid = os.fork()
            if pid == 0:
                try:
                    os.setpgid(0, 0)
                    default_signals()
                    conn.close()
                    reporter = Conn()
                    reporter.send({"type": "hello", "role": "reporter", "pid": os.getpid(), "id": msg["id"]})
                    reporter_main(reporter, msg["args"])
                finally:
                    os._exit(1)
        elif msg["type"] == "run_workers":
            pids = {}
            gc.freeze()
            try:
                for index, batch in enumerate(msg["batches"]):
                    pid = os.fork()
                    if pid == 0:
                        try:
                            default_signals()
                            conn.close()
                            worker = Conn()
                            worker.send({"type": "hello", "role": "worker", "pid": os.getpid(), "rid": msg["rid"], "index": index})
                            batch_worker(worker, root, msg["args"], batch["nodeids"], batch["files"], msg["nice"], msg["replay"])
                        finally:
                            os._exit(1)
                    pids[pid] = index
            finally:
                gc.unfreeze()
            wait_and_report(conn, msg["rid"], pids)


def spawned_main(argv):
    """A fresh interpreter for the Spawn fork point: says hello, gets config, then collects or runs one batch."""
    conn = Conn()
    if argv[0] == "reporter":
        conn.send({"type": "hello", "role": "reporter", "pid": os.getpid(), "id": int(argv[1])})
        reporter_main(conn, json.loads(argv[2]))
    if argv[0] == "l1":
        conn.send({"type": "hello", "role": "l1", "pid": os.getpid(), "cid": int(argv[1])})
    else:
        conn.send({"type": "hello", "role": "worker", "pid": os.getpid(), "rid": int(argv[1]), "index": int(argv[2])})
    config = conn.recv()
    msg = conn.recv()
    if config is None or msg is None:
        os._exit(0)
    if argv[0] == "l1":
        l1_main(conn, config["root"], msg)
    default_signals()
    batch_worker(conn, config["root"], msg["args"], msg["nodeids"], msg["files"], msg["nice"], msg["replay"])


if __name__ == "__main__":
    if sys.argv[1:2] == ["l0"]:
        l0_main()
    elif sys.argv[1:2] in (["l1"], ["worker"], ["reporter"]):
        spawned_main(sys.argv[1:])
    else:
        sys.exit(f"usage: {sys.argv[0]} l0 | l1 CID | worker RID INDEX | reporter ID ARGS_JSON")
