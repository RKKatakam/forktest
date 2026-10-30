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
