"""JAX starts native thread pools when arrays are built at import; forktest must never fork a process holding them."""

import os
import shutil
import time
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

from conftest import REPO, Project, done, failed, selection

JAX_PYTHON = Path(os.environ.get("FORKTEST_JAX_PYTHON") or "/tmp/jaxprobe/.venv/bin/python")
FORK_WARNING = "os.fork() was called"

pytestmark = pytest.mark.skipif(not JAX_PYTHON.exists(), reason=f"no JAX interpreter at {JAX_PYTHON}")


@pytest.fixture
def jaxdemo(tmp_path, forktest_bin) -> Project:
    shutil.copytree(REPO / "examples" / "jaxdemo", tmp_path / "jaxdemo")
    return Project(tmp_path / "jaxdemo", forktest_bin, python=str(JAX_PYTHON))


def test_all_on_four_workers_matches_pytest_and_an_edit_reruns_the_callers(jaxdemo):
    expected = jaxdemo.pytest_counts()
    started = time.monotonic()
    events, rc = jaxdemo.forktest("--all", "-n", "4")
    assert time.monotonic() - started < 60
    assert rc == 0 and FORK_WARNING not in jaxdemo.stderr, jaxdemo.stderr
    assert {"passed": done(events).passed, "failed": done(events).failed} == {"passed": expected["passed"], "failed": 0}
    assert jaxdemo.log("collected") == ["forktest: collected 8 tests; workers fork from imported"]

    jaxdemo.edit("model/layers.py", "return jnp.tanh(w @ x)", "return jnp.tanh(w @ x + 0.0)")
    events, rc = jaxdemo.forktest("-n", "4", "--junitxml", "out.xml")
    assert rc == 0 and FORK_WARNING not in jaxdemo.stderr, jaxdemo.stderr
    assert selection(events).changed == ["model/layers.py"]
    assert set(selection(events).reasons) == {"coverage"}
    assert failed(events) == set() and done(events).passed == selection(events).selected
    reran = {c.attrib["name"] for c in ET.parse(jaxdemo.root / "out.xml").getroot().iter("testcase")}
    assert {"test_dense_shape", "test_jit_matches_eager", "test_dense_zero_input"} <= reran, reran


DENSE = "return jnp.tanh(w @ x)"


def test_an_edit_run_whose_workers_collect_their_own_files_starts_no_l1_and_honours_x(jaxdemo):
    events, rc = jaxdemo.forktest("-x")
    assert rc == 0 and done(events).passed == 8
    assert jaxdemo.log("collected") == ["forktest: collected 8 tests; workers fork from imported"]
    rechecks = 0

    def l1_skipped_or_checked_once(selected: int) -> None:
        # Linux numpy starts an OpenBLAS pool that L0 tolerates once a run has taught it to preimport numpy; the
        # stored fork point is then checked once more in L1, and trusted again after that.
        nonlocal rechecks
        no_l1 = jaxdemo.log("collected") == [] and len(jaxdemo.log(f"no L1: {selected} tests from the inventory")) == 1
        checked = len(jaxdemo.log("the stored fork point was found with")) == 1
        rechecks += checked
        collected = jaxdemo.log("collected")
        assert no_l1 or (checked and len(collected) == 1 and collected[0].endswith("workers fork from imported")), jaxdemo.stderr
        assert rechecks <= 1, jaxdemo.stderr

    jaxdemo.edit("model/layers.py", DENSE, DENSE + " + 0.0")
    events, rc = jaxdemo.forktest("-x")
    assert rc == 0 and FORK_WARNING not in jaxdemo.stderr, jaxdemo.stderr
    l1_skipped_or_checked_once(7)
    assert selection(events).reasons == {"coverage": 7} and done(events).passed == 7 and failed(events) == set()

    jaxdemo.edit("model/layers.py", DENSE + " + 0.0", DENSE + " * jnp.nan")
    events, rc = jaxdemo.forktest("-x")
    assert FORK_WARNING not in jaxdemo.stderr, jaxdemo.stderr
    l1_skipped_or_checked_once(selection(events).selected)
    assert rc == 1 and len(failed(events)) == 1 and done(events).failed == 1, events


def test_a_selected_test_the_workers_no_longer_find_is_reported(jaxdemo):
    jaxdemo.write(
        "tests/test_case.py",
        "import os\n\nimport jax.numpy as jnp\nimport pytest\n\nfrom model.layers import dense\n\n\n"
        "@pytest.mark.parametrize('case', [os.environ.get('JAXDEMO_CASE', 'a')])\n"
        "def test_case(case):\n    assert dense(jnp.eye(2), jnp.zeros(2)).shape == (2,)\n",
    )
    events, rc = jaxdemo.forktest()
    assert rc == 0 and done(events).passed == 9
    jaxdemo.edit("model/layers.py", DENSE, DENSE + " + 0.0")
    events, rc = jaxdemo.forktest(JAXDEMO_CASE="b")
    assert len(jaxdemo.log("no L1: 8 tests from the inventory")) == 1, jaxdemo.stderr
    assert failed(events) == {"tests/test_case.py::test_case[a]"} and done(events).passed == 7, events
    lost = next(e for e in events if getattr(e, "nodeid", None) == "tests/test_case.py::test_case[a]")
    assert "did not collect" in lost.message
