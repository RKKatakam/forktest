"""tests/test_daemon.py against the Rust binary: the daemon, its warm zygote, watch mode, and clean exits."""

import json
import os
import shutil
import signal
import subprocess
import threading
import time
from pathlib import Path

import pytest

from conftest import REPO, Project, done, failed, parse, selection

ROUND = "return amount.quantize(quantum, rounding=ROUND_HALF_UP)"
JAX_PYTHON = Path("/tmp/jaxprobe/.venv/bin/python")
FORK_WARNING = "os.fork() was called"
SHIM = "forktest_shim"


def wait_for(predicate, timeout=30, what="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {what}")


def status(project: Project) -> dict | None:
    proc = project.run("daemon", "status")
    if proc.returncode:
        return None
    return dict(line.split(": ", 1) for line in proc.stdout.splitlines())


def zygote(project: Project) -> tuple[str, str] | None:
    st = status(project)
    return st and (st["zygote"], st["zygote_pid"])


def wait_ready(project: Project, other_than: str | None = None, what="zygote ready") -> str:
    def ready():
        z = zygote(project)
        return z is not None and z[0] == "ready" and z[1] != other_than

    wait_for(ready, what=what)
    return zygote(project)[1]


def client(project: Project, *args: str, **env: str) -> tuple[list, int]:
    """A `--json` run that may go through the daemon; the client's stderr is in `project.stderr`."""
    proc = project.run("--json", *args, **env)
    assert "INTERNALERROR" not in proc.stderr and "forktest: error" not in proc.stderr, proc.stderr
    return [parse(line) for line in proc.stdout.splitlines()], proc.returncode


def processes() -> list[tuple[int, int, int, str]]:
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,pgid=,command="], capture_output=True, text=True).stdout
    rows = []
    for line in out.splitlines():
        pid, ppid, pgid, command = line.split(None, 3)
        rows.append((int(pid), int(ppid), int(pgid), command))
    return rows


def tree(root_pid: int) -> set[int]:
    """`root_pid` and every process below it."""
    rows = processes()
    found = {root_pid}
    while True:
        more = {pid for pid, ppid, _, _ in rows if ppid in found} - found
        if not more:
            return found
        found |= more


def survivors(pids: set[int]) -> list[str]:
    """Processes that are one of `pids`, or in one of their process groups."""
    return [f"{pid} {command}" for pid, _, pgid, command in processes() if pid in pids or pgid in pids]


def safety_nets(out: list[str]) -> int:
    return sum(line.startswith("safety net:") for line in out)


def daemon_log(project: Project) -> str:
    return (project.root / ".forktest" / "daemon.log").read_text()


def start(project: Project, **env: str) -> dict:
    proc = project.run("daemon", "start", **env)
    assert proc.returncode == 0 and "started (pid" in proc.stdout, proc.stdout + proc.stderr
    wait_ready(project)
    return status(project)


def stop_and_check_nothing_survives(project: Project) -> None:
    daemon = int(status(project)["pid"])
    pids = tree(daemon)
    assert any(SHIM in command for pid, _, _, command in processes() if pid in pids), "the daemon runs Python children"
    stop = project.run("daemon", "stop")
    assert "stopped" in stop.stdout, stop.stdout
    wait_for(lambda: not survivors(pids), timeout=10, what="daemon processes to exit")
    assert not (project.root / ".forktest" / "daemon.sock").exists()
    assert not (project.root / ".forktest" / "daemon.pid").exists()
    assert project.run("daemon", "status").returncode == 1


@pytest.fixture
def served(fresh):
    yield fresh
    fresh.run("daemon", "stop")


def test_daemon_serves_incremental_runs_and_stops_clean(served):
    demo = served
    assert demo.run("daemon", "status").returncode == 1
    st = start(demo)
    l1 = st["zygote_pid"]
    assert int(st["imported"]) > 0 and st["tests"] == "16"

    events, rc = client(demo)
    assert rc == 0
    assert selection(events).reasons == {"new": 16} and done(events).passed == 16
    assert zygote(demo) == ("ready", l1), "an unchanged tree keeps the same zygote"

    demo.edit("billing/tax.py", ROUND, ROUND + " + 1")
    wait_ready(demo, other_than=l1, what="zygote rebuilt")
    events, rc = client(demo)
    assert selection(events).reasons == {"coverage": 7} and rc == 1

    events, _ = client(demo, "--all", "-k", "tax")
    assert selection(events).total == 6
    assert status(demo)["pytest_args"] == '["-k","tax"]'

    assert demo.run("daemon", "start").stdout.startswith("forktest daemon: already running")
    stop_and_check_nothing_survives(demo)


def test_an_edit_run_waits_for_the_warm_zygote_and_collects_nothing(served):
    demo = served
    start(demo, FORKTEST_LOG="1")
    _, rc = client(demo)
    assert rc == 0
    l1 = zygote(demo)[1]

    demo.edit("billing/tax.py", ROUND, ROUND + " + 1")
    wait_ready(demo, other_than=l1, what="zygote rebuilt")
    before = len(daemon_log(demo))
    events, rc = client(demo, FORKTEST_LOG="1")
    assert "served by the daemon" in demo.stderr, demo.stderr
    assert selection(events).reasons == {"coverage": 7} and rc == 1 and done(events).failed == 7
    served_log = daemon_log(demo)[before:]
    assert "served by the warm collection of 16 tests" in served_log, served_log
    assert "collect" not in served_log.replace("warm collection", ""), served_log


def test_an_edit_run_right_after_the_save_sees_the_new_code(served):
    demo = served
    start(demo)
    client(demo)
    current = ""
    for delta in [" + 1", " + 22", ""]:
        demo.edit("billing/tax.py", ROUND + current + "\n", ROUND + delta + "\n")
        current = delta
        events, rc = client(demo)
        assert selection(events).changed == ["billing/tax.py"], delta
        assert (rc, done(events).failed) == ((1, 7) if delta else (0, 0)), (delta, events)


def test_a_new_test_file_is_seen_by_the_next_run(served):
    demo = served
    start(demo)
    client(demo)
    demo.write("tests/test_new.py", "def test_new():\n    assert True\n")
    events, rc = client(demo)
    assert rc == 0 and selection(events).reasons == {"new": 1} and selection(events).total == 17
    demo.root.joinpath("tests/test_new.py").unlink()
    events, rc = client(demo)
    assert rc == 0 and selection(events).total == 16


def test_no_daemon_and_another_directory_run_without_it(served):
    demo = served
    start(demo)
    proc = demo.run("--json", "--no-daemon", FORKTEST_LOG="1")
    assert proc.returncode == 0 and "served by the daemon" not in proc.stderr
    proc = subprocess.run(
        [demo.binary, "--json", "tests/test_tax.py"], cwd=demo.root / "tests", capture_output=True, text=True,
        env=demo.env(FORKTEST_LOG="1"),
    )
    assert "served by the daemon" not in proc.stderr, proc.stderr


def test_a_run_survives_the_daemon_stopping_under_it(served):
    demo = served
    demo.write("tests/test_sleepy.py", "import time\n\ndef test_sleepy():\n    time.sleep(1.5)\n")
    start(demo)
    proc = subprocess.Popen(
        [demo.binary, "--json"], cwd=demo.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env=demo.env(),
    )
    time.sleep(0.6)
    assert "stopped" in demo.run("daemon", "stop").stdout
    out, err = proc.communicate(timeout=60)
    events = [parse(line) for line in out.splitlines()]
    last = len(events) - 1 - [type(e).__name__ for e in events][::-1].index("Selection")
    assert proc.returncode == 0, out + err
    assert "running without it" in err, err
    finished = done(events[last:])
    assert (finished.failed, finished.errors) == (0, 0) and finished.passed >= 1, out
    events, rc = demo.forktest()
    assert rc == 0 and selection(events).selected == 0, "every test ran to the end once"


def test_the_zygote_dying_mid_run_loses_no_results_and_is_rebuilt(served):
    demo = served
    demo.write("tests/test_sleepy.py", "import time\n\ndef test_sleepy():\n    time.sleep(1.5)\n")
    l1 = start(demo)["zygote_pid"]
    proc = subprocess.Popen(
        [demo.binary, "--json"], cwd=demo.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env=demo.env(),
    )
    time.sleep(0.6)
    os.kill(int(l1), signal.SIGKILL)
    out, err = proc.communicate(timeout=60)
    events = [parse(line) for line in out.splitlines()]
    assert proc.returncode == 0 and done(events).passed == 17, out + err
    wait_ready(demo, other_than=l1, what="zygote rebuilt after it died")


def test_watch_reruns_affected_tests_on_save_and_exits_clean(demo):
    proc = subprocess.Popen(
        [demo.binary, "watch"], cwd=demo.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        start_new_session=True, env=demo.env(),
    )
    out: list[str] = []
    reader = threading.Thread(target=lambda: out.extend(proc.stdout), daemon=True)
    reader.start()
    try:
        l1 = wait_ready(demo, what="watch zygote ready")
        wait_for(lambda: safety_nets(out) == 1, what="the first watch run")
        demo.edit("data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
        wait_for(lambda: any("1 of 16 tests selected" in line for line in out), what="watch run after a data edit")
        wait_for(lambda: safety_nets(out) == 2, what="its safety net")
        assert zygote(demo) == ("ready", l1), "a data-file edit keeps the zygote"
        demo.edit("config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
        wait_ready(demo, other_than=l1, what="zygote rebuilt after a .py edit")
        wait_for(lambda: any("12 of 16 tests selected" in line for line in out), what="watch run after a .py edit")
        wait_for(lambda: safety_nets(out) == 3, what="its safety net")
        pids = tree(proc.pid)
    finally:
        os.kill(proc.pid, signal.SIGINT)
        _, err = proc.communicate(timeout=30)
        reader.join(timeout=5)
    text = "".join(out)
    assert proc.returncode == 0, err
    assert "1 of 16 tests selected" in text and "test_rate_for_country" in text, text
    assert "12 of 16 tests selected" in text and "test_currency" in text, text
    assert "0 misses" in text and "MISS" not in text, text
    assert "zygote ready" in err and "stale" in err, err
    wait_for(lambda: not survivors(pids), timeout=10, what="watch processes to exit")


def test_watch_cancels_a_run_when_a_new_change_arrives(demo):
    demo.write("tests/test_sleepy.py", "import time\nfrom config import CURRENCY\n\ndef test_sleepy():\n    time.sleep(3)\n")
    proc = subprocess.Popen(
        [demo.binary, "watch"], cwd=demo.root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        start_new_session=True, env=demo.env(),
    )
    out: list[str] = []
    reader = threading.Thread(target=lambda: out.extend(proc.stdout), daemon=True)
    reader.start()
    try:
        wait_for(lambda: safety_nets(out) == 1, timeout=60, what="the first watch run")
        demo.edit("config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
        wait_for(lambda: any("13 of 17 tests selected" in line for line in out), what="a run that includes the slow test")
        time.sleep(0.5)
        started = time.monotonic()
        demo.edit("config.py", 'CURRENCY = "EURO"', 'CURRENCY = "EUR"')
        wait_for(lambda: sum("tests selected" in line for line in out) == 3, what="the next run")
        assert time.monotonic() - started < 2.5, "the slow run was cancelled, not waited for"
    finally:
        os.kill(proc.pid, signal.SIGINT)
        _, err = proc.communicate(timeout=30)
    assert proc.returncode == 0, err
    assert "run cancelled by a new change" in err, err


def test_watch_refuses_to_start_next_to_a_daemon(served):
    start(served)
    proc = served.run("watch", timeout=10)
    assert proc.returncode == 1 and "already running" in proc.stderr


@pytest.mark.skipif(not JAX_PYTHON.exists(), reason=f"no JAX interpreter at {JAX_PYTHON}")
def test_the_jax_demo_under_the_daemon_serves_edit_runs_without_forking_threads(tmp_path, forktest_bin):
    jaxdemo = Project(tmp_path / "jaxdemo", forktest_bin, python=str(JAX_PYTHON))
    shutil.copytree(REPO / "examples" / "jaxdemo", jaxdemo.root)
    try:
        st = start(jaxdemo)
        assert st["fork_point"] == "imported", st
        events, rc = client(jaxdemo, "-n", "4")
        assert rc == 0 and done(events).passed == 8 and failed(events) == set(), events
        layers = "return jnp.tanh(w @ x)"
        for n, (old, new) in enumerate([(layers, layers + " + 0.0"), (layers + " + 0.0", layers + " * 1.00")]):
            l1 = zygote(jaxdemo)[1]
            jaxdemo.edit("model/layers.py", old, new)
            if n:
                wait_ready(jaxdemo, other_than=l1, what="JAX zygote rebuilt")
            events, rc = client(jaxdemo, "-n", "4")
            assert rc == 0 and selection(events).changed == ["model/layers.py"], (n, events, jaxdemo.stderr)
            assert set(selection(events).reasons) == {"coverage"}, n
            assert failed(events) == set() and done(events).passed == selection(events).selected > 0, n
        log = daemon_log(jaxdemo)
        assert FORK_WARNING not in log and FORK_WARNING not in jaxdemo.stderr, log
        stop_and_check_nothing_survives(jaxdemo)
    finally:
        jaxdemo.run("daemon", "stop")
