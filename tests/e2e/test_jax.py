"""JAX starts native thread pools when arrays are built at import; forktest must never fork a process holding them."""

import shutil
import time
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

from conftest import REPO, Project, done, failed, selection

JAX_PYTHON = Path("/tmp/jaxprobe/.venv/bin/python")
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
