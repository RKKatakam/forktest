"""Which interpreter runs the tests, and what happens when it cannot run forktest's shim."""

import os
import shutil
import subprocess

import pytest

from conftest import DEMO


def project_with_venv(tmp_path, python_version, *packages):
    root = tmp_path / "proj"
    shutil.copytree(DEMO, root, ignore=shutil.ignore_patterns(".forktest", "__pycache__"))
    if shutil.which("uv") is None:
        pytest.skip("uv is not installed")
    made = subprocess.run(["uv", "venv", "-q", "--python", python_version, str(root / ".venv")], capture_output=True)
    if made.returncode:
        pytest.skip(f"no Python {python_version} for uv")
    if packages:
        installed = subprocess.run(["uv", "pip", "install", "-q", "--python", str(root / ".venv/bin/python"), *packages])
        if installed.returncode:
            pytest.skip("could not install packages")
    return root


def run(forktest_bin, root, *args):
    env = {k: v for k, v in os.environ.items() if k not in ("VIRTUAL_ENV", "FORKTEST_PYTHON", "PYTHONPATH")}
    return subprocess.run([forktest_bin, *args], cwd=root, capture_output=True, text=True, env=env, timeout=300)


def test_the_project_venv_is_found_without_activation(forktest_bin, tmp_path):
    root = project_with_venv(tmp_path, "3.12", "pytest")
    proc = run(forktest_bin, root)
    assert proc.returncode == 0, proc.stderr
    assert "16 passed" in proc.stdout


def test_a_python_older_than_3_12_runs_plain_pytest(forktest_bin, tmp_path):
    root = project_with_venv(tmp_path, "3.11", "pytest")
    proc = run(forktest_bin, root, "-q")
    assert "forktest needs 3.12 or newer; running plain pytest" in proc.stderr
    assert proc.returncode == 0 and "16 passed" in proc.stdout


def test_a_venv_without_pytest_gets_pytest_s_own_error_and_a_hint(forktest_bin, tmp_path):
    root = project_with_venv(tmp_path, "3.12")
    proc = run(forktest_bin, root)
    assert "pytest is not installed" in proc.stderr and "No module named pytest" in proc.stderr
    assert proc.returncode == 1
