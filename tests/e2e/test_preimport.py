"""Preimports learned from one run must not break the next."""

from conftest import done


def make_libs(tmp_path):
    libs = tmp_path / "libs"
    (libs / "plugpkg").mkdir(parents=True)
    (libs / "plugpkg" / "__init__.py").write_text(
        "import pytest\n\n\n@pytest.fixture\ndef plugged():\n    return 42\n"
    )
    (libs / "wrapper").mkdir()
    (libs / "wrapper" / "__init__.py").write_text("import plugpkg\n\nVALUE = 1\n")
    return libs


def test_a_preimport_that_pulls_in_a_rewritten_plugin_is_dropped(fresh, tmp_path):
    libs = make_libs(tmp_path)
    fresh.write("conftest.py", (fresh.root / "conftest.py").read_text() + "\nimport wrapper\n")
    fresh.write("tests/test_plugged.py", "def test_plugged(plugged):\n    assert plugged == 42\n")
    args = ("-p", "plugpkg", "-W", "error::pytest.PytestAssertRewriteWarning")
    for run in range(3):
        events, rc = fresh.forktest("--all", *args, PYTHONPATH=str(libs))
        assert rc == 0, (run, fresh.stderr)
        assert done(events).passed == 17, run


CONFTEST_THREAD = "\nimport threading\nimport time\n\nthreading.Thread(target=time.sleep, args=(3600,), daemon=True).start()\n"


def set_stored_tolerated_threads(project, n):
    """What the store says L0 tolerated when it found the fork point; numpy's pool is 32 on a 32-core Linux box."""
    import json
    import sqlite3

    db = sqlite3.connect(project.root / ".forktest" / "forktest.db")
    with db:
        for key, value in db.execute("select key, value from meta where key like 'args:%'").fetchall():
            meta = json.loads(value)
            meta["fork_point_tolerated"] = n
            db.execute("update meta set value = ? where key = ?", (json.dumps(meta), key))
    db.close()


def test_a_stored_fork_point_found_with_other_tolerated_threads_is_checked_again_in_l1(fresh):
    fresh.write("conftest.py", (fresh.root / "conftest.py").read_text() + CONFTEST_THREAD)
    events, rc = fresh.forktest()
    assert rc == 0 and fresh.log("collected 16 tests; workers fork from imported"), fresh.stderr
    fresh.edit("billing/tax.py", "ROUND_HALF_UP)", "ROUND_HALF_EVEN)")
    events, rc = fresh.forktest()
    assert fresh.log("no L1: 7 tests"), "found with the tolerated threads L0 has now, it is trusted"

    set_stored_tolerated_threads(fresh, 32)
    fresh.edit("billing/tax.py", "ROUND_HALF_EVEN)", "ROUND_HALF_UP)")
    events, rc = fresh.forktest()
    assert rc == 0 and done(events).passed == 7, fresh.stderr
    assert not fresh.log("no L1") and fresh.log("collected 7 tests; workers fork from imported"), fresh.stderr

    fresh.edit("billing/tax.py", "ROUND_HALF_UP)", "ROUND_HALF_EVEN)")
    events, rc = fresh.forktest()
    assert fresh.log("no L1: 7 tests"), "the check in L1 recorded what it found"
