"""Local time in a forked worker: as fast as in an unforked process, and the same answers."""

from conftest import done

LOCAL_TIME_TEST = '''
import datetime
import os
import subprocess
import sys
import time

PROBE = "import datetime, time; print(([datetime.datetime.fromtimestamp(t).isoformat() for t in (0, 1e9, 1.18e9, 1.7e9)], time.tzname))"


def test_local_time_matches_a_fresh_interpreter():
    here = str(([datetime.datetime.fromtimestamp(t).isoformat() for t in (0, 1e9, 1.18e9, 1.7e9)], time.tzname))
    fresh = subprocess.run([sys.executable, "-c", PROBE], capture_output=True, text=True, check=True).stdout.strip()
    assert here == fresh


def test_now_is_cheap():
    start = time.perf_counter()
    for _ in range(2000):
        datetime.datetime.now()
    assert time.perf_counter() - start < 0.1


def test_the_fix_is_not_visible_to_tests():
    assert "forktest_tzfix" not in os.environ.get("DYLD_INSERT_LIBRARIES", "")
'''


def test_forked_workers_keep_local_time_fast_and_correct(fresh):
    fresh.write("tests/test_local_time.py", LOCAL_TIME_TEST)
    events, rc = fresh.forktest("tests/test_local_time.py")
    assert rc == 0, fresh.stderr
    assert done(events).passed == 3
