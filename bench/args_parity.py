"""pytest and forktest with the same arguments, from a cold forktest store: exit codes and counts must agree.

usage: python bench/args_parity.py CORPUS_DIR FORKTEST_BIN [PROJECT ...]
"""

import os
import re
import subprocess
import sys
from pathlib import Path

CORPUS = Path(sys.argv[1])
FORKTEST = sys.argv[2]
ONLY = set(sys.argv[3:])

COMMON = [
    [],
    ["-k", "not slow and not network"],
    ["-x"],
    ["--maxfail=2"],
    ["-v"],
    ["-q", "-rA"],
    ["--tb=short"],
    ["-p", "no:cacheprovider"],
    ["-o", "addopts="],
    ["-W", "ignore::DeprecationWarning"],
    ["--durations=3"],
    ["-n", "2"],
    ["-n4"],
    ["--strict-markers"],
    ["--junitxml", "/tmp/ft-args-junit.xml"],
]

PER_PROJECT = {
    "demo": [["tests/test_tax.py"], ["tests/test_tax.py::test_vat_for"], ["-m", "slow"], ["--deselect", "tests/test_tax.py::test_vat_for"]],
    "flask": [["tests/test_basic.py"], ["-k", "session"], ["--ignore", "tests/test_basic.py"], ["tests/test_json.py", "tests/test_views.py"]],
    "attrs": [["tests/test_make.py", "-k", "Validator"], ["--ignore-glob", "*mypy*"], ["-m", "not hypothesis"]],
    "click": [["tests/test_options.py"], ["-k", "help"], ["tests/test_basic.py::test_basic_functionality"]],
    "jaxdemo": [["-n", "4"], ["tests/test_eager.py"]],
}

COUNT = re.compile(r"(\d+) (passed|failed|skipped|xfailed|xpassed|errors?)\b")


def summary(output: str) -> dict[str, int]:
    for line in reversed(output.splitlines()):
        found = COUNT.findall(line)
        if found and re.search(r" in [\d.]+s", line):
            return {kind.rstrip("s") if kind.startswith("error") else kind: int(n) for n, kind in found}
    return {}


def run(cmd: list[str], root: Path) -> tuple[int, dict[str, int], str]:
    env = {**os.environ, "VIRTUAL_ENV": str(root / ".venv"), "PATH": f"{root / '.venv/bin'}:{os.environ['PATH']}"}
    env.pop("PYTHONPATH", None)
    proc = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=900)
    return proc.returncode, summary(proc.stdout + proc.stderr), proc.stdout + proc.stderr


def main() -> None:
    failures = 0
    total = 0
    for name in [n for n in ("demo", "jaxdemo", "flask", "attrs", "click") if not ONLY or n in ONLY]:
        root = CORPUS / name
        python = str(root / ".venv/bin/python")
        has_xdist = subprocess.run([python, "-c", "import xdist"], capture_output=True).returncode == 0
        for args in COMMON + PER_PROJECT.get(name, []):
            # click's addopts deselect 31,000 slow tests; clearing them makes plain pytest run for many minutes.
            if name == "click" and args == ["-o", "addopts="]:
                continue
            if not has_xdist and any(a.startswith("-n") for a in args):
                continue
            for db in (root / ".forktest").glob("forktest.db*"):
                db.unlink()
            p_code, p_counts, _ = run([python, "-m", "pytest", *args], root)
            f_code, f_counts, f_out = run([FORKTEST, *args], root)
            ok = p_code == f_code and p_counts == f_counts
            total += 1
            failures += not ok
            mark = "ok  " if ok else "DIFF"
            print(f"{mark} {name:8} {' '.join(args) or '(none)':48} pytest {p_code} {p_counts}  forktest {f_code} {f_counts}", flush=True)
            if not ok:
                print("     " + "\n     ".join(f_out.strip().splitlines()[-6:]))
    print(f"\n{total - failures}/{total} argument sets agree")


main()
