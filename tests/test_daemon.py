"""Daemon and watch mode, driven through the CLI on a scratch copy of examples/demo."""

import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

from forktest.events import Done, Selection, parse

DEMO = Path(__file__).resolve().parent.parent / "examples" / "demo"
FORKTEST = [sys.executable, "-m", "forktest"]


def run(root, *args, timeout=60):
    proc = subprocess.run([*FORKTEST, *args], cwd=root, capture_output=True, text=True, timeout=timeout)
    assert "INTERNALERROR" not in proc.stderr, proc.stderr
    return proc


def events_of(proc):
    return [parse(line) for line in proc.stdout.splitlines()]


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def processes_in_group(pgid: int) -> list[str]:
    out = subprocess.run(["ps", "-axo", "pid=,pgid=,command="], capture_output=True, text=True).stdout
    return [line for line in out.splitlines() if line.split()[1:2] == [str(pgid)]]


def wait_for(predicate, timeout=30, what="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {what}")


def status(root) -> dict | None:
    proc = run(root, "daemon", "status")
    if proc.returncode:
        return None
    return dict(line.split(": ", 1) for line in proc.stdout.splitlines())


@pytest.fixture
def demo(tmp_path):
    shutil.copytree(DEMO, tmp_path / "demo")
    root = tmp_path / "demo"
    yield root
    run(root, "daemon", "stop")


def edit(root, rel, old, new):
    path = root / rel
    text = path.read_text()
    assert old in text and len(old) != len(new)
    path.write_text(text.replace(old, new))


def test_daemon_serves_incremental_runs_and_stops_clean(demo):
    assert run(demo, "daemon", "status").returncode == 1
    proc = run(demo, "daemon", "start")
    assert proc.returncode == 0 and "started (pid" in proc.stdout
    wait_for(lambda: (status(demo) or {}).get("zygote") == "ready", what="zygote ready")
    st = status(demo)
    l0, l1 = int(st["pid"]), int(st["zygote_pid"])
    assert int(st["imported"]) > 0

    first = events_of(run(demo, "--json"))
    assert next(e for e in first if isinstance(e, Selection)).reasons == {"new": 16}
    assert next(e for e in first if isinstance(e, Done)).passed == 16
    assert status(demo)["zygote_pid"] == str(l1), "an unchanged tree keeps the same zygote"

    edit(demo, "billing/tax.py", "return amount.quantize(quantum, rounding=ROUND_HALF_UP)",
         "return amount.quantize(quantum, rounding=ROUND_HALF_UP) + 1")
    wait_for(lambda: (status(demo) or {}).get("zygote_pid") not in (str(l1), "None"), what="zygote rebuilt")
    wait_for(lambda: (status(demo) or {}).get("zygote") == "ready", what="new zygote ready")
    second = run(demo, "--json")
    sel = next(e for e in events_of(second) if isinstance(e, Selection))
    assert sel.reasons == {"coverage": 7} and second.returncode == 1

    third = events_of(run(demo, "--json", "--all", "-k", "tax"))
    assert next(e for e in third if isinstance(e, Selection)).total == 6
    assert status(demo)["pytest_args"] == "['-k', 'tax']"

    assert run(demo, "daemon", "start").stdout.startswith("forktest daemon: already running")
    stop = run(demo, "daemon", "stop")
    assert "stopped" in stop.stdout
    wait_for(lambda: not alive(l0) and not alive(l1), what="daemon processes to exit")
    assert processes_in_group(l1) == [] and processes_in_group(l0) == []
    assert not (demo / ".forktest" / "daemon.sock").exists()
    assert run(demo, "daemon", "status").returncode == 1


def test_watch_reruns_affected_tests_on_save_and_exits_clean(demo):
    run(demo)
    proc = subprocess.Popen(
        [*FORKTEST, "watch"], cwd=demo, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        start_new_session=True,
    )
    try:
        wait_for(lambda: (status(demo) or {}).get("zygote") == "ready", what="watch zygote ready")
        l1 = int(status(demo)["zygote_pid"])
        edit(demo, "data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
        wait_for(lambda: (demo / ".forktest" / "store.db").stat().st_mtime > time.time() - 2, timeout=20, what="watch run")
        time.sleep(1.5)
        assert status(demo)["zygote_pid"] == str(l1), "a data-file edit keeps the zygote"
        edit(demo, "config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
        wait_for(lambda: (status(demo) or {}).get("zygote_pid") not in (str(l1), "None"), what="zygote rebuilt after .py edit")
        wait_for(lambda: (status(demo) or {}).get("zygote") == "ready", what="rebuilt zygote ready")
        time.sleep(1.5)
    finally:
        os.kill(proc.pid, signal.SIGINT)
        out, err = proc.communicate(timeout=30)
    assert proc.returncode == 0, err
    assert "1 of 16 tests selected" in out and "test_rate_for_country" in out
    assert "12 of 16 tests selected" in out and "test_currency" in out
    assert "0 misses" in out
    assert "zygote ready" in err and "stale" in err
    wait_for(lambda: not alive(proc.pid) and not alive(l1), what="watch processes to exit")
    assert processes_in_group(proc.pid) == []
