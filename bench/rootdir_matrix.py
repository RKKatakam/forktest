"""Compare forktest's project root with pytest's rootdir over a matrix of layouts.

Each case builds a layout in a temp dir, runs `pytest` (with a plugin that writes `config.rootpath` and exits) and
`forktest --print-root` from the same directory with the same arguments, and prints both. Exits 1 on any mismatch.

    .venv/bin/python bench/rootdir_matrix.py [--bin target/release/forktest]
"""

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path

PROBE = """
import os

def pytest_addoption(parser):
    parser.addoption("--probe-opt")

def pytest_configure(config):
    with open(os.environ["ROOTPROBE_OUT"], "w") as f:
        f.write(str(config.rootpath))
    os._exit(0)
"""

TEST = "def test_x():\n    pass\n"
PYPROJECT = "[project]\nname = 'x'\n"

# (name, files, cwd relative to the layout, args, extra env). A file maps to its text; a name ending in / is a dir.
CASES = [
    ("no config", {"tests/test_a.py": TEST}, ".", [], {}),
    ("no config, from a subdir", {"tests/test_a.py": TEST}, "tests", [], {}),
    ("no config, arg into a subdir", {"tests/sub/test_a.py": TEST}, ".", ["tests/sub"], {}),
    ("no config, arg to a sibling of cwd", {"a/test_a.py": TEST, "b/test_b.py": TEST}, "a", ["../b"], {}),
    ("no config, two sibling args", {"a/test_a.py": TEST, "b/test_b.py": TEST}, ".", ["a", "b"], {}),
    ("pytest.ini, from a subdir", {"pytest.ini": "[pytest]\n", "tests/test_a.py": TEST}, "tests", [], {}),
    ("empty pytest.ini", {"pytest.ini": "", "tests/test_a.py": TEST}, "tests", [], {}),
    (".pytest.ini", {".pytest.ini": "", "tests/test_a.py": TEST}, "tests", [], {}),
    ("pytest.toml", {"pytest.toml": "[pytest]\n", "tests/test_a.py": TEST}, "tests", [], {}),
    ("pyproject, no pytest table, from a subdir", {"pyproject.toml": PYPROJECT, "tests/test_a.py": TEST}, "tests", [], {}),
    (
        "pyproject [tool.pytest.ini_options]",
        {"pyproject.toml": PYPROJECT, "pkg/pyproject.toml": "[tool.pytest.ini_options]\naddopts = '-q'\n", "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "pyproject [tool.pytest] with a key",
        {"pyproject.toml": PYPROJECT, "pkg/pyproject.toml": "[tool.pytest]\naddopts = ['-q']\n", "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "pyproject empty [tool.pytest]",
        {"setup.cfg": "[tool:pytest]\n", "pkg/pyproject.toml": "[tool.pytest]\n", "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "pyproject pytest table only in a comment",
        {"tox.ini": "[pytest]\n", "pkg/pyproject.toml": "# [tool.pytest.ini_options]\n[project]\nname = 'x'\n", "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "nearest pyproject without a table, setup.py above",
        {"setup.py": "", "pkg/pyproject.toml": PYPROJECT, "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "pyproject dotted key under [tool]",
        {"tox.ini": "[pytest]\n", "pkg/pyproject.toml": "[tool]\npytest.ini_options.addopts = '-q'\n", "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "pyproject table header inside a multi-line string",
        {"tox.ini": "[pytest]\n", "pkg/pyproject.toml": '[project]\nreadme = """\n[tool.pytest.ini_options]\n"""\n', "pkg/t/test_a.py": TEST},
        "pkg/t", [], {},
    ),
    (
        "setup.py above the args beats config inside them",
        {"setup.py": "", "a/pytest.ini": "", "a/test_a.py": TEST, "b/test_b.py": TEST},
        ".", ["a", "b"], {},
    ),
    ("setup.cfg [tool:pytest]", {"pyproject.toml": PYPROJECT, "pkg/setup.cfg": "[tool:pytest]\n", "pkg/t/test_a.py": TEST}, "pkg/t", [], {}),
    ("setup.cfg without [tool:pytest]", {"pyproject.toml": PYPROJECT, "pkg/setup.cfg": "[metadata]\nname = x\n", "pkg/t/test_a.py": TEST}, "pkg/t", [], {}),
    ("tox.ini [pytest]", {"pyproject.toml": PYPROJECT, "pkg/tox.ini": "[tox]\n[pytest]\n", "pkg/t/test_a.py": TEST}, "pkg/t", [], {}),
    ("tox.ini without [pytest]", {"pyproject.toml": PYPROJECT, "pkg/tox.ini": "[tox]\n# [pytest]\n", "pkg/t/test_a.py": TEST}, "pkg/t", [], {}),
    ("setup.py only, from a subdir", {"setup.py": "", "tests/test_a.py": TEST}, "tests", [], {}),
    (".git only, from a subdir", {".git/": None, "tests/test_a.py": TEST}, "tests", [], {}),
    (".git and pyproject, from a subdir", {".git/": None, "pyproject.toml": PYPROJECT, "tests/test_a.py": TEST}, "tests", [], {}),
    (
        "nested config, arg into the inner project",
        {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST},
        ".", ["pkg/test_a.py"], {},
    ),
    (
        "nested config, nodeid arg",
        {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST},
        ".", ["pkg/test_a.py::test_x"], {},
    ),
    (
        "config in two sibling args",
        {"a/pytest.ini": "", "a/test_a.py": TEST, "b/pytest.ini": "", "b/test_b.py": TEST},
        ".", ["b", "a"], {},
    ),
    ("arg that does not exist", {"pkg/pytest.ini": "", "pkg/test_a.py": TEST}, ".", ["pkg/missing.py"], {}),
    ("--rootdir=", {"pytest.ini": "", "sub/test_a.py": TEST}, ".", ["--rootdir=sub"], {}),
    ("--rootdir with a separate value", {"pytest.ini": "", "sub/test_a.py": TEST}, ".", ["--rootdir", "sub"], {}),
    ("--rootdir with a variable", {"pytest.ini": "", "sub/test_a.py": TEST}, ".", ["--rootdir=$ROOTVAR/sub"], {"ROOTVAR": "."}),
    ("-c other.ini", {"pytest.ini": "", "configs/other.ini": "[pytest]\n", "tests/test_a.py": TEST}, ".", ["-c", "configs/other.ini"], {}),
    ("--config-file=", {"pytest.ini": "", "configs/other.ini": "[pytest]\n", "tests/test_a.py": TEST}, "tests", ["--config-file=../configs/other.ini"], {}),
    ("-cFILE", {"pytest.ini": "", "configs/other.ini": "[pytest]\n", "tests/test_a.py": TEST}, ".", ["-cconfigs/other.ini"], {}),
    ("-c /dev/null", {"pytest.ini": "", "tests/test_a.py": TEST}, "tests", ["-c", "/dev/null"], {}),
    ("-c and --rootdir", {"configs/other.ini": "", "tests/test_a.py": TEST}, ".", ["-c", "configs/other.ini", "--rootdir=tests"], {}),
    ("--deselect value is not a path arg", {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST}, ".", ["--deselect", "pkg/test_a.py::test_x"], {}),
    ("-k value is not a path arg", {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST}, ".", ["-k", "pkg"], {}),
    ("-xk cluster takes a value", {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST}, ".", ["-xk", "pkg"], {}),
    ("unknown option's value counts as a path", {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST}, ".", ["--probe-opt", "pkg"], {}),
    ("args after --", {"pytest.ini": "", "pkg/pytest.ini": "", "pkg/test_a.py": TEST}, ".", ["--", "pkg"], {}),
    ("PYTEST_ADDOPTS", {"pytest.ini": "", "sub/test_a.py": TEST}, ".", [], {"PYTEST_ADDOPTS": "--rootdir sub"}),
]


def build(base: Path, files: dict) -> None:
    for rel, text in files.items():
        p = base / rel
        if rel.endswith("/"):
            p.mkdir(parents=True, exist_ok=True)
            continue
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)


def show(path: str | None, base: Path) -> str:
    if path is None:
        return "error"
    real = Path(os.path.realpath(path))
    if real == base or base in real.parents:
        return str(real.relative_to(base))
    return str(real)


def pytest_root(python: str, cwd: Path, args: list, env: dict, probe_dir: Path) -> str | None:
    out = probe_dir / "rootpath"
    out.unlink(missing_ok=True)
    env = {**env, "PYTEST_PLUGINS": "rootprobe", "PYTHONPATH": str(probe_dir), "ROOTPROBE_OUT": str(out)}
    subprocess.run([python, "-m", "pytest", "-p", "no:cacheprovider", *args], cwd=cwd, env=env, capture_output=True)
    return out.read_text() if out.exists() else None


def forktest_root(binary: str, cwd: Path, args: list, env: dict) -> str | None:
    proc = subprocess.run([binary, "--print-root", *args], cwd=cwd, env=env, capture_output=True, text=True)
    return proc.stdout.strip() if proc.returncode == 0 and proc.stdout.strip() else None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", default=os.environ.get("FORKTEST_BIN", "target/release/forktest"))
    parser.add_argument("--python", default=sys.executable)
    opts = parser.parse_args()
    binary = os.path.abspath(opts.bin)
    mismatches = 0
    rows = []
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(os.path.realpath(tmp))
        probe_dir = tmp / "_probe"
        probe_dir.mkdir()
        (probe_dir / "rootprobe.py").write_text(PROBE)
        for i, (name, files, cwd, args, extra) in enumerate(CASES):
            base = tmp / f"case{i}"
            build(base, files)
            env = {k: v for k, v in os.environ.items() if not k.startswith(("PYTEST_", "FORKTEST_"))}
            env.update(extra)
            want = show(pytest_root(opts.python, base / cwd, args, env, probe_dir), base)
            got = show(forktest_root(binary, base / cwd, args, env), base)
            ok = want == got
            mismatches += not ok
            rows.append((name, f"{cwd} $ {' '.join(args)}".rstrip(" $"), want, got, "ok" if ok else "MISMATCH"))
    header = ("case", "invocation", "pytest", "forktest", "")
    widths = [max(len(r[i]) for r in [header, *rows]) for i in range(5)]
    for row in [header, *rows]:
        print("  ".join(c.ljust(w) for c, w in zip(row, widths)).rstrip())
    print(f"\n{len(rows) - mismatches}/{len(rows)} match")
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main())
