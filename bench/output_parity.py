"""Plain pytest against forktest --all, case by case: terminal output, exit code and JUnit XML.

usage: python bench/output_parity.py PROJECT [--scenario] [--only NAME ...] [--case NAME=ARGS ...]

PROJECT holds a .venv with pytest. forktest is $FORKTEST_BIN. Each case runs both sides with the same pytest args
plus -p no:cacheprovider and --junitxml, then compares stdout with timings normalised and forktest's headline dropped,
the exit codes, and the XML with time, timestamp and hostname removed. --scenario first writes a test file with a
failure that prints and logs, a fixture error, a teardown failure, a warning, an xfail and an xpass, and a module that
skips itself; the collection-error case adds a module that fails to import for that case alone.
"""

import difflib
import os
import re
import shlex
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

CASES = {
    "default": [],
    "-q": ["-q"],
    "-v": ["-v"],
    "-rA": ["-rA"],
    "--tb=short": ["--tb=short"],
    "--tb=line": ["--tb=line"],
    "--tb=no": ["--tb=no"],
    "--show-capture=no": ["--show-capture=no"],
    "-s": ["-s"],
    "-x": ["-x"],
}

SCENARIO = {
    "tests/test_parity_cases.py": '''import logging
import sys
import warnings

import pytest


@pytest.fixture
def broken():
    raise RuntimeError("fixture setup failed")


@pytest.fixture
def bad_teardown():
    yield 1
    raise RuntimeError("teardown failed")


def test_prints_and_fails():
    print("hello from stdout")
    sys.stderr.write("hello from stderr\\n")
    logging.getLogger("parity").warning("a log line")
    assert 1 == 2


def test_fixture_error(broken):
    pass


def test_teardown_failure(bad_teardown):
    assert bad_teardown == 1


def test_warns():
    warnings.warn("careful", UserWarning)


@pytest.mark.xfail(reason="known bug")
def test_xfail():
    assert False


@pytest.mark.xfail(reason="maybe fixed")
def test_xpass():
    assert True


@pytest.mark.skip(reason="not today")
def test_skipped():
    pass
''',
    "tests/test_parity_skipmod.py": '''import pytest

pytest.skip("this module is skipped", allow_module_level=True)


def test_never():
    pass
''',
}

BROKEN = ("tests/test_parity_broken.py", "import nothing_here_at_all\n\n\ndef test_x():\n    pass\n")

TIMING = re.compile(r" in \d+\.\d+s( \(\d+:\d\d:\d\d\))?")


def normalise(text: str, forktest: bool) -> list[str]:
    lines = text.splitlines()
    if forktest and lines and lines[0].startswith("forktest · "):
        lines = lines[1:]
    return [TIMING.sub(" in Xs", line) for line in lines]


def junit(path: Path) -> list[str]:
    if not path.exists():
        return ["<no file>"]
    root = ET.parse(path).getroot()
    for el in root.iter():
        for attr in ("time", "timestamp", "hostname"):
            el.attrib.pop(attr, None)
    ET.indent(root)
    return ET.tostring(root, encoding="unicode").splitlines()


def run(cmd: list[str], cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env={**os.environ, **env}, timeout=900)


def compare(project: Path, name: str, args: list[str], out: Path) -> bool:
    python = str(project / ".venv/bin/python")
    common = [*args, "-p", "no:cacheprovider"]
    # One path for both sides: pytest prints it.
    xml = out / f"{name}.xml"
    theirs_xml, ours_xml = out / f"{name}.pytest.xml", out / f"{name}.forktest.xml"
    for p in (xml, theirs_xml, ours_xml):
        p.unlink(missing_ok=True)
    theirs = run([python, "-m", "pytest", *common, f"--junitxml={xml}"], project, {})
    if xml.exists():
        xml.rename(theirs_xml)
    ours = run(
        [os.environ["FORKTEST_BIN"], "--no-daemon", "--all", *common, "--junitxml", str(xml)],
        project,
        {"FORKTEST_PYTHON": python},
    )
    if xml.exists():
        xml.rename(ours_xml)
    problems = []
    if theirs.returncode != ours.returncode:
        problems.append(f"exit code: pytest {theirs.returncode}, forktest {ours.returncode}")
    a, b = normalise(theirs.stdout, False), normalise(ours.stdout, True)
    if a != b:
        problems.append("\n".join(difflib.unified_diff(a, b, "pytest", "forktest", lineterm="", n=1)))
    a, b = junit(theirs_xml), junit(ours_xml)
    if a != b:
        problems.append("JUnit:\n" + "\n".join(difflib.unified_diff(a, b, "pytest", "forktest", lineterm="", n=1)))
    noise = [line for line in ours.stderr.splitlines() if line.strip()]
    print(f"{'PASS' if not problems else 'DIFF'}  {name}", flush=True)
    for p in problems:
        print("    " + p.replace("\n", "\n    "))
    if noise and problems:
        print("    forktest stderr: " + " | ".join(noise[:5]))
    return not problems


def main() -> int:
    argv = sys.argv[1:]
    project = Path(argv.pop(0)).resolve()
    cases = dict(CASES)
    only: list[str] = []
    scenario = False
    while argv:
        flag = argv.pop(0)
        if flag == "--scenario":
            scenario = True
        elif flag == "--only":
            only.append(argv.pop(0))
        elif flag == "--case":
            name, _, args = argv.pop(0).partition("=")
            cases[name] = shlex.split(args)
        else:
            sys.exit(f"unknown option {flag}")
    if scenario:
        for rel, text in SCENARIO.items():
            (project / rel).write_text(text)
        cases["collection error"] = []
    out = Path(tempfile.mkdtemp(prefix="output-parity-"))
    results = {}
    for name, args in cases.items():
        if only and name not in only:
            continue
        broken = project / BROKEN[0]
        if name == "collection error":
            broken.write_text(BROKEN[1])
        try:
            results[name] = compare(project, name.replace(" ", "_").replace("/", "_"), args, out)
        finally:
            broken.unlink(missing_ok=True)
    passed = sum(results.values())
    print(f"\n{passed}/{len(results)} cases match on {project}")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
