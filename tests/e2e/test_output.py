"""The human output and --junitxml are pytest's own, printed by a pytest session that replays the run."""

import re
import xml.etree.ElementTree as ET

from conftest import Project
from test_daemon import daemon_log, served, start  # noqa: F401

CASES = """import logging
import sys
import warnings

import pytest


@pytest.fixture
def broken():
    raise RuntimeError("fixture setup failed")


def test_prints_and_fails():
    print("hello from stdout")
    sys.stderr.write("hello from stderr\\n")
    logging.getLogger("demo").warning("a log line")
    assert 1 == 2


def test_fixture_error(broken):
    pass


def test_warns():
    warnings.warn("careful", UserWarning)
"""

SKIPPED_MODULE = 'import pytest\n\npytest.skip("not on this platform", allow_module_level=True)\n\n\ndef test_x():\n    pass\n'


def normalised(text: str) -> list[str]:
    return [re.sub(r" in \d+\.\d+s", " in Xs", line) for line in text.splitlines() if not line.startswith("forktest · ")]


def test_the_output_is_pytests_with_forktests_headline_first(fresh: Project):
    fresh.write("tests/test_cases.py", CASES)
    ours = fresh.run("--no-daemon", "--all", "-p", "no:cacheprovider")
    theirs = fresh.pytest("-p", "no:cacheprovider")
    lines = ours.stdout.splitlines()
    assert lines[0].startswith("forktest · all 19 tests selected") and "test session starts" in lines[1], ours.stdout
    for shown in ("Captured stdout call", "hello from stdout", "Captured log call", "a log line", "warnings summary",
                  "UserWarning: careful", "ERROR at setup of test_fixture_error", "short test summary info"):
        assert shown in ours.stdout, shown
    assert (ours.returncode, normalised(ours.stdout)) == (theirs.returncode, normalised(theirs.stdout))


def test_an_edit_run_counts_the_tests_it_did_not_select_as_deselected(fresh: Project):
    fresh.run("--no-daemon")
    fresh.edit("billing/tax.py", "ROUND_HALF_UP", "ROUND_HALF_EVEN")
    proc = fresh.run("--no-daemon", "-p", "no:cacheprovider")
    assert "collected 16 items / " in proc.stdout and " deselected / " in proc.stdout, proc.stdout
    assert re.search(r"=+ \d+ (passed|failed).* deselected.* in \d", proc.stdout.splitlines()[-1]), proc.stdout


def test_pytests_junit_reports_a_module_level_skip(fresh: Project):
    fresh.write("tests/test_platform.py", SKIPPED_MODULE)
    proc = fresh.run("--no-daemon", "--all", "--junitxml", "out.xml")
    assert proc.returncode == 0, proc.stdout + proc.stderr
    suite = ET.parse(fresh.root / "out.xml").getroot().find("testsuite")
    assert suite.attrib["tests"] == "17" and suite.attrib["skipped"] == "1"
    case = next(c for c in suite.iter("testcase") if c.attrib["name"] == "tests.test_platform")
    assert case.find("skipped").attrib["message"] == "collection skipped"
    assert "1 skipped" in proc.stdout.splitlines()[-1]


def test_json_with_junitxml_prints_only_json_and_writes_pytests_xml(fresh: Project):
    fresh.write("tests/test_platform.py", SKIPPED_MODULE)
    proc = fresh.run("--no-daemon", "--all", "--json", "--junitxml", "out.xml")
    assert all(line.startswith('{"event":') for line in proc.stdout.splitlines()), proc.stdout
    assert ET.parse(fresh.root / "out.xml").getroot().find("testsuite").attrib["skipped"] == "1"


def test_a_reporter_that_dies_leaves_forktests_own_report(fresh: Project):
    with open(fresh.root / "conftest.py", "a") as f:
        f.write("\n\ndef pytest_terminal_summary(terminalreporter):\n    import os\n    os._exit(7)\n")
    fresh.edit("billing/tax.py", "ROUND_HALF_UP", "ROUND_HALF_EVEN")
    proc = fresh.run("--no-daemon", "--all")
    assert "pytest's reporter" in proc.stderr and "showing forktest's own report" in proc.stderr, proc.stderr
    assert re.search(r"^\d+ failed, \d+ passed in ", proc.stdout.splitlines()[-1]), proc.stdout
    assert proc.returncode == 1


def test_the_daemon_hands_its_reporter_the_clients_terminal(served):  # noqa: F811
    start(served)
    served.edit("billing/tax.py", "ROUND_HALF_UP", "ROUND_HALF_EVEN")
    proc = served.run(FORKTEST_LOG="1")
    assert "served by the daemon" in proc.stderr, proc.stderr
    lines = proc.stdout.splitlines()
    assert lines[0].startswith("forktest · no history · 16 of 16") and "test session starts" in lines[1], proc.stdout
    assert "short test summary info" in proc.stdout and proc.returncode == 1


def test_the_daemon_prepares_the_next_runs_reporter_and_drops_it_when_a_conftest_changes(served):  # noqa: F811
    start(served, FORKTEST_LOG="1")
    served.run()
    served.edit("billing/tax.py", "ROUND_HALF_UP", "ROUND_HALF_EVEN")
    before = len(daemon_log(served))
    proc = served.run()
    assert "forktest: pytest's reporter was prepared" in daemon_log(served)[before:]
    assert "test session starts" in proc.stdout, proc.stdout
    with open(served.root / "conftest.py", "a") as f:
        f.write("\n\ndef pytest_report_header():\n    return 'from the new conftest'\n")
    before = len(daemon_log(served))
    proc = served.run()
    assert "forktest: pytest's reporter starts now" in daemon_log(served)[before:]
    assert "from the new conftest" in proc.stdout, proc.stdout


TERMINAL_WRITER = """def test_writes_to_the_terminal_mid_test(request):
    request.config.get_terminal_writer().line("written mid-test")


def test_last_failed():
    assert False
"""


def test_a_test_can_write_through_the_terminal_writer_and_lf_sees_the_failure(fresh: Project):
    fresh.write("tests/test_terminal.py", TERMINAL_WRITER)
    ours = fresh.run("--no-daemon", "--all", "tests/test_terminal.py")
    assert "1 failed, 1 passed" in ours.stdout and "FAILED tests/test_terminal.py::test_writes" not in ours.stdout, ours.stdout
    rerun = fresh.run("--lf", "tests/test_terminal.py")
    assert "1 failed" in rerun.stdout and "1 passed" not in rerun.stdout, rerun.stdout


PARTLY_COVERED = "def used(flag):\n    if flag:\n        return 1\n    return 2\n\n\ndef unused():\n    return 3\n"
USES_IT = "from billing.extra import used\n\n\ndef test_used():\n    assert used(True) == 1\n"


def coverage_section(text: str) -> list[str]:
    lines = text.splitlines()
    start = next(i for i, line in enumerate(lines) if "coverage: platform" in line)
    end = next(i for i, line in enumerate(lines) if line.startswith("TOTAL"))
    return lines[start : end + 1]


def test_cov_reports_what_the_workers_measured_as_pytest_does(fresh: Project):
    fresh.write("billing/extra.py", PARTLY_COVERED)
    fresh.write("tests/test_extra.py", USES_IT)
    args = ("-p", "no:cacheprovider", "--cov=billing", "--cov-branch", "--cov-report=term-missing", "--cov-fail-under=100")
    theirs = fresh.pytest(*args)
    for fork_from in ("collected", "imported"):
        ours = fresh.run("--no-daemon", "--all", "--fork-from", fork_from, *args)
        assert ours.returncode == theirs.returncode == 1, ours.stdout + ours.stderr
        assert coverage_section(ours.stdout) == coverage_section(theirs.stdout), ours.stdout
        assert "Required test coverage of 100% not reached" in ours.stdout
    assert not list(fresh.root.glob(".coverage.*"))


def test_a_file_given_twice_with_keep_duplicates_runs_twice_as_in_pytest(fresh: Project):
    fresh.write("tests/test_dup.py", "def test_ok():\n    pass\n\n\ndef test_bad():\n    assert False\n")
    args = ("-p", "no:cacheprovider", "--keep-duplicates", "tests/test_dup.py", "tests/test_dup.py")
    ours, theirs = fresh.run("--no-daemon", *args), fresh.pytest(*args)
    assert (ours.returncode, normalised(ours.stdout)) == (theirs.returncode, normalised(theirs.stdout)), ours.stdout
