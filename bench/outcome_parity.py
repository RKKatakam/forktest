"""Per-test outcomes of pytest and `forktest --all` on each corpus project, from JUnit XML.

usage: python bench/outcome_parity.py CORPUS_DIR FORKTEST_BIN [PROJECT ...]

Each project runs once under pytest and once under forktest from a cold store, both writing JUnit XML. Every
testcase is keyed by (classname, name) and reduced to passed, failed, error, skipped or xfailed. The script prints
every test whose outcome differs or that only one side reports, and exits 1 when any project differs.
"""

import os
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from pathlib import Path

CORPUS = Path(sys.argv[1])
FORKTEST = sys.argv[2]
ONLY = set(sys.argv[3:])

PROJECTS = {
    "demo": [],
    "jaxdemo": [],
    "more-itertools": [],
    "attrs": [],
    "click": [],
    "flask": [],
    "rich": [],
    "pandas": ["pandas/tests/frame"],
    "fastapi": [],
    "django-rest-framework": [],
}


def outcomes(path: Path) -> dict[tuple[str, str], str]:
    out = {}
    for case in ET.parse(path).getroot().iter("testcase"):
        kind = "passed"
        for child in case:
            if child.tag in ("failure", "error"):
                kind = "failed" if child.tag == "failure" else "error"
            elif child.tag == "skipped":
                kind = "xfailed" if child.get("type") == "pytest.xfail" else "skipped"
        key = (case.get("classname", ""), case.get("name", ""))
        # pytest writes a test that fails in call and errors in teardown twice; the worse outcome wins.
        if out.get(key) not in ("failed", "error"):
            out[key] = kind
    return out


def run(cmd: list[str], root: Path) -> tuple[float, int, str]:
    env = {**os.environ, "VIRTUAL_ENV": str(root / ".venv"), "PATH": f"{root / '.venv/bin'}:{os.environ['PATH']}"}
    env.pop("PYTHONPATH", None)
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=1800)
    return time.perf_counter() - t0, proc.returncode, (proc.stdout + proc.stderr)[-1500:]


def main() -> None:
    differing = 0
    for name, args in PROJECTS.items():
        root = CORPUS / name
        if (ONLY and name not in ONLY) or not (root / ".venv").exists():
            continue
        for db in (root / ".forktest").glob("forktest.db*"):
            db.unlink()
        pxml, fxml = Path(f"/tmp/parity-{name}-pytest.xml"), Path(f"/tmp/parity-{name}-forktest.xml")
        pt, pcode, pout = run([str(root / ".venv/bin/python"), "-m", "pytest", "-q", "-p", "no:cacheprovider", f"--junitxml={pxml}", *args], root)
        ft, fcode, fout = run([FORKTEST, "-q", "--all", "--junitxml", str(fxml), *args], root)
        if not pxml.exists() or not fxml.exists():
            differing += 1
            print(f"{name}: no JUnit XML (pytest exit {pcode}, forktest exit {fcode})\n{pout}\n{fout}")
            continue
        p, f = outcomes(pxml), outcomes(fxml)
        diffs = sorted(
            (k, p.get(k, "absent"), f.get(k, "absent")) for k in p.keys() | f.keys() if p.get(k) != f.get(k)
        )
        status = "same" if not diffs and pcode == fcode else "DIFF"
        differing += status == "DIFF"
        print(f"{status} {name:22} {len(p):6} tests  exit {pcode}/{fcode}  pytest {pt:6.2f}s  forktest --all (cold) {ft:6.2f}s  {len(diffs)} differ")
        for (cls, test), po, fo in diffs[:15]:
            print(f"     {cls}::{test}: pytest {po}, forktest {fo}")
        if len(diffs) > 15:
            print(f"     ... {len(diffs) - 15} more")
    sys.exit(1 if differing else 0)


main()
