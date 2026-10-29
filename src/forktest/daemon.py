"""Warm zygotes behind a unix socket, and the watch loop.

L0 is the long-lived process: it preimports third-party modules, owns the
socket, polls the file tree, and manages one L1. L1 is forked from L0 and
runs ``pytest.main`` with the zygote plugin; it collects once and then
serves run requests until L0 declares it stale and replaces it.
"""

from __future__ import annotations

import enum
import json
import os
import selectors
import signal
import socket
import sys
import time
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any, NoReturn

from forktest.events import CollectError, Done, Event, Request, dumps, parse
from forktest.reporters import HumanReporter
from forktest.selection import is_config_file, is_excluded

POLL_INTERVAL = 0.3
DEBOUNCE = 0.15
RESTART_FILES = ("pyproject.toml", "uv.lock", "poetry.lock")


@dataclass(frozen=True)
class Paths:
    root: str

    @property
    def dir(self) -> str:
        return os.path.join(self.root, ".forktest")

    @property
    def sock(self) -> str:
        """AF_UNIX paths are capped near 104 bytes on macOS, so deep checkouts get a path under TMPDIR."""
        in_project = os.path.join(self.dir, "daemon.sock")
        if len(in_project.encode()) <= 96:
            return in_project
        import hashlib
        import tempfile

        digest = hashlib.sha1(os.path.realpath(self.root).encode()).hexdigest()[:12]
        return os.path.join(tempfile.gettempdir(), f"forktest-{digest}.sock")

    @property
    def pid(self) -> str:
        return os.path.join(self.dir, "daemon.pid")

    @property
    def log(self) -> str:
        return os.path.join(self.dir, "daemon.log")


def send_frame(sock: socket.socket, obj: Any, fds: list[int] = []) -> None:
    body = json.dumps(obj).encode()
    header = len(body).to_bytes(4, "big")
    if fds:
        socket.send_fds(sock, [header], fds)
    else:
        sock.sendall(header)
    sock.sendall(body)


def recv_frame(sock: socket.socket) -> tuple[Any, list[int]]:
    """Return ``(message, fds)``; ``message`` is ``None`` at EOF."""
    header, fds, _, _ = socket.recv_fds(sock, 4, 8)
    while header and len(header) < 4:
        more = sock.recv(4 - len(header))
        if not more:
            return None, fds
        header += more
    if not header:
        return None, fds
    size = int.from_bytes(header, "big")
    body = b""
    while len(body) < size:
        chunk = sock.recv(size - len(body))
        if not chunk:
            return None, fds
        body += chunk
    return json.loads(body), fds


def log(msg: str) -> None:
    sys.stderr.write(f"[forktest {time.strftime('%H:%M:%S')}] {msg}\n")
    sys.stderr.flush()


_runner: Any = None
L0_SIGNALS = {signal.SIGTERM, signal.SIGINT, signal.SIGHUP}


def _l1_sigterm(signum: int, frame: Any) -> NoReturn:
    if _runner is not None:
        _runner._kill_workers()
    os._exit(0)


def l1_main(control: socket.socket, pytest_args: list[str]) -> NoReturn:
    os.setpgid(0, 0)
    signal.signal(signal.SIGTERM, _l1_sigterm)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGHUP, signal.SIG_DFL)
    signal.pthread_sigmask(signal.SIG_UNBLOCK, L0_SIGNALS)
    import pytest

    from forktest.zygote import ZygotePlugin

    served = False

    def loop(runner: Any) -> None:
        nonlocal served
        served = True
        serve_forever(runner, control)

    rc = pytest.main(pytest_args, plugins=[ZygotePlugin(loop)])
    if not served:
        try:
            send_frame(control, {"state": "failed", "code": int(rc)})
        except OSError:
            pass
    os._exit(0)


def serve_forever(runner: Any, control: socket.socket) -> None:
    from forktest.zygote import Cancelled, ClientGone

    global _runner
    _runner = runner
    send_frame(control, {"state": "ready", "imported": runner.imported_files()})
    pending: deque[tuple[Any, list[int]]] = deque()

    def should_cancel() -> bool:
        msg, fds = recv_frame(control)
        if msg is not None and msg["op"] == "cancel":
            return True
        pending.append((msg, fds))
        return msg is None or msg["op"] == "shutdown"

    while True:
        msg, fds = pending.popleft() if pending else recv_frame(control)
        if msg is None or msg["op"] == "shutdown":
            return
        if msg["op"] != "run":
            continue
        client = socket.socket(fileno=fds[0])

        def emit(event: Event) -> None:
            try:
                client.sendall(dumps(event).encode() + b"\n")
            except OSError as exc:
                raise ClientGone from exc

        try:
            runner.run(Request.from_json(msg["request"]), emit, control.fileno(), should_cancel)
        except (Cancelled, ClientGone):
            pass
        finally:
            client.close()
        if pending and pending[0][0] is None:
            return
        send_frame(control, {"state": "idle", "imported": runner.imported_files()})



class L1State(enum.Enum):
    ABSENT = "absent"
    BUILDING = "building"
    READY = "ready"
    STALE = "stale"


@dataclass
class L1:
    state: L1State = L1State.ABSENT
    pid: int = 0
    control: socket.socket | None = None
    args: tuple[str, ...] = ()
    imported: set[str] = field(default_factory=set)
    outstanding: int = 0
    failures: int = 0


class Watcher:
    def __init__(self, root: str) -> None:
        self.root = root
        self.snapshot = self.scan()

    def scan(self) -> dict[str, tuple[int, int]]:
        out: dict[str, tuple[int, int]] = {}
        for dirpath, dirnames, filenames in os.walk(self.root):
            dirnames[:] = [d for d in dirnames if not d.startswith(".") and not is_excluded(d)]
            rel_dir = os.path.relpath(dirpath, self.root)
            for name in filenames:
                rel = name if rel_dir == "." else f"{rel_dir}/{name}"
                try:
                    st = os.stat(os.path.join(dirpath, name))
                except OSError:
                    continue
                out[rel] = (st.st_mtime_ns, st.st_size)
        return out

    def poll(self) -> set[str]:
        current = self.scan()
        changed = {p for p in current.keys() | self.snapshot.keys() if current.get(p) != self.snapshot.get(p)}
        self.snapshot = current
        return changed


def _preimport(root: str) -> int:
    import importlib

    from forktest.store import Store

    store = Store(os.path.join(root, ".forktest", "store.db"))
    try:
        names = json.loads(store.meta_get("preimport") or "[]")
    finally:
        store.close()
    count = 0
    for name in names:
        try:
            importlib.import_module(name)
            count += 1
        except Exception:  # the list came from an earlier environment; a name that no longer imports is not an error
            pass
    return count


class Daemon:
    def __init__(self, root: str, pytest_args: list[str], *, listen: bool, watch: bool, workers: int = 0) -> None:
        self.root = root
        self.paths = Paths(root)
        self.pytest_args = tuple(pytest_args)
        self.listen = listen
        self.watch = watch
        self.workers = workers
        self.started = time.time()
        self.sel = selectors.DefaultSelector()
        self.l1 = L1()
        self.queue: deque[tuple[socket.socket, dict[str, Any]]] = deque()
        self.watcher = Watcher(root)
        self.listener: socket.socket | None = None
        self.watch_pending_at: float | None = None
        self.watch_stream: socket.socket | None = None
        self.watch_reporter: HumanReporter | None = None
        self.watch_buf = b""
        self.preimported = 0

    def run(self) -> None:
        os.makedirs(self.paths.dir, exist_ok=True)
        for sig in L0_SIGNALS:
            signal.signal(sig, self._on_signal)
        if self.listen:
            self._bind()
        self.preimported = _preimport(self.root)
        log(f"daemon up (pid {os.getpid()}), {self.preimported} third-party modules preimported")
        self.build_l1(self.pytest_args)
        try:
            while True:
                for key, _ in self.sel.select(POLL_INTERVAL):
                    key.data()
                self._reap()
                self._poll_files()
                self._dispatch()
        except SystemExit:
            pass
        finally:
            self.shutdown()

    def _on_signal(self, signum: int, frame: Any) -> None:
        raise SystemExit(0)

    def _bind(self) -> None:
        if os.path.exists(self.paths.sock):
            os.unlink(self.paths.sock)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(self.paths.sock)
        self.listener.listen(16)
        self.sel.register(self.listener, selectors.EVENT_READ, self._accept)
        with open(self.paths.pid, "w") as f:
            f.write(str(os.getpid()))

    def shutdown(self) -> None:
        log("shutting down")
        self.kill_l1("shutdown")
        self._reap(block=True)
        if self.listener is not None:
            self.listener.close()
            for p in (self.paths.sock, self.paths.pid):
                try:
                    os.unlink(p)
                except OSError:
                    pass
        for client, _ in self.queue:
            client.close()

    def build_l1(self, args: tuple[str, ...]) -> None:
        assert self.l1.state == L1State.ABSENT
        parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        import gc

        gc.freeze()
        signal.pthread_sigmask(signal.SIG_BLOCK, L0_SIGNALS)
        pid = os.fork()
        gc.unfreeze()
        if pid == 0:
            parent.close()
            for sock in (self.listener, self.watch_stream, *(c for c, _ in self.queue)):
                if sock is not None:
                    sock.close()
            l1_main(child, list(args))
        try:
            os.setpgid(pid, pid)
        except OSError:  # the child got there first, or already exited
            pass
        signal.pthread_sigmask(signal.SIG_UNBLOCK, L0_SIGNALS)
        child.close()
        self.l1 = L1(L1State.BUILDING, pid, parent, args, failures=self.l1.failures)
        self.sel.register(parent, selectors.EVENT_READ, self._read_l1)
        log(f"zygote building (pid {pid}, pytest args {list(args) or 'default'})")

    def kill_l1(self, reason: str) -> None:
        if self.l1.state in (L1State.ABSENT, L1State.STALE):
            return
        log(f"zygote {self.l1.pid} stale: {reason}")
        self.l1.state = L1State.STALE
        try:
            os.killpg(self.l1.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        self._end_watch_stream(cancelled=True)

    def _read_l1(self) -> None:
        assert self.l1.control is not None
        msg, _ = recv_frame(self.l1.control)
        if msg is None:
            self._l1_gone()
            return
        if msg["state"] == "failed":
            self.l1.failures += 1
            log(f"zygote exited before collecting (pytest exit code {msg['code']}); see {self.paths.log}")
            self._fail_queue(f"zygote failed to start (pytest exit code {msg['code']}); see {self.paths.log}")
            return
        self.l1.imported = set(msg.get("imported", ()))
        if msg["state"] == "ready":
            self.l1.failures = 0
            log(f"zygote ready (pid {self.l1.pid}), {len(self.l1.imported)} project files imported")
        else:
            self.l1.outstanding -= 1
        if self.l1.state == L1State.BUILDING:
            self.l1.state = L1State.READY

    def _l1_gone(self) -> None:
        if self.l1.control is not None:
            self.sel.unregister(self.l1.control)
            self.l1.control.close()
        if self.l1.state != L1State.STALE:
            self.l1.failures += 1
            log(f"zygote {self.l1.pid} exited unexpectedly")
        self._end_watch_stream(cancelled=True)
        self.l1 = L1(L1State.ABSENT, failures=self.l1.failures)

    def _reap(self, block: bool = False) -> None:
        while True:
            try:
                pid, _ = os.waitpid(-1, 0 if block else os.WNOHANG)
            except ChildProcessError:
                return
            if pid == 0:
                return
            if pid == self.l1.pid and self.l1.state != L1State.ABSENT:
                self._l1_gone()

    def _accept(self) -> None:
        assert self.listener is not None
        client, _ = self.listener.accept()
        self.sel.register(client, selectors.EVENT_READ, lambda: self._read_request(client))

    def _read_request(self, client: socket.socket) -> None:
        self.sel.unregister(client)
        try:
            msg, _ = recv_frame(client)
        except OSError:
            msg = None
        if msg is None:
            client.close()
            return
        op = msg.get("op")
        if op == "run":
            self.queue.append((client, msg["request"]))
        elif op == "status":
            send_frame(client, self.status())
            client.close()
        elif op == "stop":
            client.close()
            raise SystemExit(0)
        else:
            client.close()

    def status(self) -> dict[str, Any]:
        return {
            "pid": os.getpid(),
            "root": self.root,
            "uptime_s": round(time.time() - self.started, 1),
            "preimported": self.preimported,
            "zygote": self.l1.state.value,
            "zygote_pid": self.l1.pid or None,
            "imported": len(self.l1.imported),
            "pytest_args": list(self.l1.args),
            "watch": self.watch,
        }

    def _fail_queue(self, message: str) -> None:
        while self.queue:
            client, _ = self.queue.popleft()
            try:
                client.sendall(dumps(CollectError("<zygote>", message)).encode() + b"\n")
                client.sendall(dumps(Done(errors=1)).encode() + b"\n")
            except OSError:
                pass
            client.close()

    def _dispatch(self) -> None:
        if self.l1.state == L1State.ABSENT:
            wants = self.queue or self.watch_pending_at is not None or self.listen
            if wants and self.l1.failures < 3:
                args = tuple(self.queue[0][1]["pytest_args"]) if self.queue else self.pytest_args
                self.build_l1(args)
            elif self.queue:
                self._fail_queue("zygote keeps failing to start; see " + self.paths.log)
            return
        if self.l1.state != L1State.READY:
            return
        if self.queue and tuple(self.queue[0][1]["pytest_args"]) != self.l1.args:
            self.kill_l1("different pytest args requested")
            return
        while self.queue:
            client, request = self.queue.popleft()
            self._send_run(request, client)
            client.close()
        if self.watch_pending_at is not None and time.monotonic() - self.watch_pending_at >= DEBOUNCE:
            self.watch_pending_at = None
            self._start_watch_run()

    def _send_run(self, request: dict[str, Any], client: socket.socket) -> None:
        assert self.l1.control is not None
        send_frame(self.l1.control, {"op": "run", "request": request}, [client.fileno()])
        self.l1.outstanding += 1

    def _poll_files(self) -> None:
        before = set(self.watcher.snapshot)
        changed = self.watcher.poll()
        if not changed:
            return
        after = set(self.watcher.snapshot)
        names = {os.path.basename(p) for p in changed}
        restart = {n for n in names if n in RESTART_FILES or (n.startswith("requirements") and n.endswith(".txt"))}
        if restart:
            log(f"{sorted(restart)} changed: restarting the daemon")
            self._restart()
        py = {p for p in changed if p.endswith(".py")}
        stale = (py & self.l1.imported) | (py & (before ^ after)) | {p for p in changed if is_config_file(p)}
        if stale and self.l1.state in (L1State.BUILDING, L1State.READY):
            self.kill_l1(f"{sorted(stale)[:3]} changed")
        if self.watch:
            log(f"changed: {', '.join(sorted(changed)[:5])}{' ...' if len(changed) > 5 else ''}")
            if self.watch_stream is not None and self.l1.state == L1State.READY:
                self._cancel_watch_run()
            self.watch_pending_at = time.monotonic()

    def _restart(self) -> NoReturn:
        self.shutdown()
        os.execv(sys.executable, [sys.executable, "-m", "forktest", *sys.argv[1:]])

    def _start_watch_run(self) -> None:
        a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        request = Request(mode="affected", verify=True, nice=True, workers=self.workers, pytest_args=list(self.l1.args))
        self.watch_reporter = HumanReporter(sys.stdout, expect_verify=True)
        self.watch_stream = a
        self.watch_buf = b""
        self.sel.register(a, selectors.EVENT_READ, self._read_watch_stream)
        self._send_run(request.to_json(), b)
        b.close()

    def _read_watch_stream(self) -> None:
        assert self.watch_stream is not None and self.watch_reporter is not None
        data = self.watch_stream.recv(1 << 16)
        if not data:
            self._end_watch_stream(cancelled=False)
            return
        self.watch_buf += data
        *lines, self.watch_buf = self.watch_buf.split(b"\n")
        for line in lines:
            self.watch_reporter.handle(parse(line))

    def _cancel_watch_run(self) -> None:
        assert self.l1.control is not None
        send_frame(self.l1.control, {"op": "cancel"})
        self._end_watch_stream(cancelled=True)

    def _end_watch_stream(self, cancelled: bool) -> None:
        if self.watch_stream is None:
            return
        self.sel.unregister(self.watch_stream)
        self.watch_stream.close()
        self.watch_stream = None
        self.watch_reporter = None
        if cancelled:
            log("run cancelled by a new change")
        else:
            sys.stdout.write("\n")
            sys.stdout.flush()



def daemon_status(root: str) -> dict[str, Any] | None:
    paths = Paths(root)
    if not os.path.exists(paths.sock):
        return None
    try:
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(5)
            sock.connect(paths.sock)
            send_frame(sock, {"op": "status"})
            msg, _ = recv_frame(sock)
            return msg
    except OSError:
        return None


def run_via_daemon(root: str, request: Request, emit: Callable[[Event], None]) -> bool:
    """Serve ``request`` through a running daemon. False when there is none to use."""
    paths = Paths(root)
    if not os.path.exists(paths.sock):
        return False
    for attempt in range(2):
        try:
            sock = socket.socket(socket.AF_UNIX)
            sock.settimeout(5)
            sock.connect(paths.sock)
        except OSError:
            return False
        sock.settimeout(None)
        send_frame(sock, {"op": "run", "request": request.to_json()})
        got_done = False
        with sock, sock.makefile("rb") as stream:
            for line in stream:
                event = parse(line)
                emit(event)
                got_done = got_done or isinstance(event, Done)
        if got_done:
            return True
        if attempt == 0:
            sys.stderr.write("forktest: the daemon's zygote restarted during the run; retrying\n")
    return True



def _parse_daemon_args(argv: list[str]) -> tuple[int, list[str]]:
    from forktest.cli import parse_args

    opts, pytest_args = parse_args(argv)
    return opts.workers, pytest_args


def _daemonize(paths: Paths) -> bool:
    """Double-fork. Returns True in the detached grandchild, False in the caller."""
    if os.fork():
        os.wait()
        return False
    os.setsid()
    if os.fork():
        os._exit(0)
    os.makedirs(paths.dir, exist_ok=True)
    log_fd = os.open(paths.log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.dup2(log_fd, 1)
    os.dup2(log_fd, 2)
    os.chdir(paths.root)
    return True


def daemon_command(argv: list[str]) -> int:
    from forktest.cli import find_root

    root = find_root(os.getcwd())
    paths = Paths(root)
    sub = argv[0] if argv else "status"
    if sub == "status":
        status = daemon_status(root)
        if status is None:
            print("forktest daemon: not running")
            return 1
        for key, value in status.items():
            print(f"{key}: {value}")
        return 0
    if sub == "stop":
        return _stop(paths)
    if sub != "start":
        print(f"forktest daemon: unknown subcommand {sub!r} (start|stop|status)", file=sys.stderr)
        return 2
    status = daemon_status(root)
    if status is not None:
        print(f"forktest daemon: already running (pid {status['pid']})")
        return 0
    workers, pytest_args = _parse_daemon_args(argv[1:])
    if _daemonize(paths):
        Daemon(root, pytest_args, listen=True, watch=False, workers=workers).run()
        os._exit(0)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        status = daemon_status(root)
        if status is not None:
            print(f"forktest daemon: started (pid {status['pid']}), log at {paths.log}")
            return 0
        time.sleep(0.05)
    print("forktest daemon: did not come up; see " + paths.log, file=sys.stderr)
    return 1


def _stop(paths: Paths) -> int:
    try:
        with open(paths.pid) as f:
            pid = int(f.read().strip())
    except (OSError, ValueError):
        print("forktest daemon: not running")
        return 0
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        for p in (paths.sock, paths.pid):
            try:
                os.unlink(p)
            except OSError:
                pass
        print("forktest daemon: not running (stale pid file removed)")
        return 0
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            print(f"forktest daemon: stopped (pid {pid})")
            return 0
        time.sleep(0.05)
    os.kill(pid, signal.SIGKILL)
    print(f"forktest daemon: killed (pid {pid})")
    return 0


def watch_command(argv: list[str]) -> int:
    from forktest.cli import find_root

    root = find_root(os.getcwd())
    if daemon_status(root) is not None:
        print("forktest watch: a daemon is already running here; stop it first (forktest daemon stop)", file=sys.stderr)
        return 1
    workers, pytest_args = _parse_daemon_args(argv)
    print(f"forktest watch: {root}", flush=True)
    daemon = Daemon(root, pytest_args, listen=True, watch=True, workers=workers)
    daemon.watch_pending_at = time.monotonic()
    daemon.run()
    return 0
