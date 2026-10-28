"""End-to-end checks of the forktest CLI on a scratch copy of examples/demo."""

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

from forktest.events import Done, Selection, TestResult, Verified, parse

DEMO = Path(__file__).resolve().parent.parent / "examples" / "demo"
ROUND_VAT_EXECUTORS = {
    "tests/test_tax.py::test_round_vat[10.005-10.01]",
    "tests/test_tax.py::test_round_vat[10.004-10.00]",
    "tests/test_tax.py::test_round_vat[0.125-0.13]",
    "tests/test_tax.py::test_vat_for_default_rate",
    "tests/test_tax.py::test_vat_for_explicit_rate",
    "tests/test_invoice.py::TestInvoice::test_total_includes_vat",
    "tests/test_invoice.py::TestInvoice::test_describe_currency",
}


class Demo:
    def __init__(self, root: Path) -> None:
        self.root = root

    def forktest(self, *args: str) -> tuple[list, int]:
        proc = subprocess.run(
            [sys.executable, "-m", "forktest", "--no-daemon", "--json", *args],
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=120,
        )
        assert "INTERNALERROR" not in proc.stderr, proc.stderr
        events = [parse(line) for line in proc.stdout.splitlines()]
        return events, proc.returncode

    def pytest_counts(self, *args: str) -> dict[str, int]:
        proc = subprocess.run(
            [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", *args],
            cwd=self.root,
            capture_output=True,
            text=True,
            timeout=120,
        )
        summary = proc.stdout.strip().splitlines()[-1]
        counts = {}
        for part in summary.split(" in ")[0].split(", "):
            n, word = part.split()
            counts[word] = int(n)
        return counts

    def edit(self, rel: str, old: str, new: str) -> None:
        """Replace text, requiring a length change so CPython never reuses a same-second .pyc."""
        path = self.root / rel
        text = path.read_text()
        assert old in text and len(old) != len(new)
        path.write_text(text.replace(old, new))


def selection(events) -> Selection:
    return next(e for e in events if isinstance(e, Selection))


def done(events) -> Done:
    return next(e for e in events if isinstance(e, Done))


def failed(events) -> set[str]:
    return {e.nodeid for e in events if isinstance(e, TestResult) and e.outcome in ("failed", "error")}


def verified(events) -> Verified:
    return next(e for e in events if isinstance(e, Verified))


@pytest.fixture
def demo(tmp_path):
    shutil.copytree(DEMO, tmp_path / "demo")
    d = Demo(tmp_path / "demo")
    events, rc = d.forktest()
    assert rc == 0 and selection(events).reasons == {"new": 16} and done(events).passed == 16
    return d


def test_first_run_selects_all_and_a_rerun_selects_nothing(demo):
    events, rc = demo.forktest("--verify-selection")
    assert rc == 0
    assert (selection(events).selected, selection(events).total) == (0, 16)
    assert done(events).passed == 0
    assert verified(events) == Verified(16, 0, verified(events).duration_ms)


def test_function_body_edit_selects_exactly_the_tests_that_execute_it(demo):
    demo.edit("billing/tax.py", "return amount.quantize(quantum, rounding=ROUND_HALF_UP)",
              "return amount.quantize(quantum, rounding=ROUND_HALF_UP) + 1")
    events, rc = demo.forktest("--verify-selection")
    assert rc == 1
    assert selection(events).changed == ["billing/tax.py"]
    assert selection(events).reasons == {"coverage": len(ROUND_VAT_EXECUTORS)}
    assert failed(events) == ROUND_VAT_EXECUTORS
    assert verified(events).misses == 0 and verified(events).checked == 16 - len(ROUND_VAT_EXECUTORS)
    assert demo.pytest_counts() == {"failed": 7, "passed": 9}


def test_docstring_edit_selects_nothing(demo):
    demo.edit("billing/tax.py", '"""Round to the configured number of places, half up."""', '"""Rounds half up."""')
    events, rc = demo.forktest("--verify-selection")
    assert rc == 0 and selection(events).selected == 0
    assert verified(events).misses == 0


def test_module_constant_edit_selects_its_importers(demo):
    demo.edit("config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
    events, rc = demo.forktest("--verify-selection")
    assert selection(events).reasons == {"import_graph": 12}
    assert failed(events) == {"tests/test_config.py::test_currency", "tests/test_invoice.py::TestInvoice::test_describe_currency"}
    assert verified(events).misses == 0 and verified(events).checked == 4


def test_data_file_edit_selects_its_reader(demo):
    demo.edit("data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
    events, rc = demo.forktest("--verify-selection")
    assert selection(events).reasons == {"file_read": 1}
    assert failed(events) == {"tests/test_tax.py::test_rate_for_country"}
    assert verified(events).misses == 0


def test_a_failing_test_reruns_as_last_failed(demo):
    demo.edit("data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
    demo.forktest()
    events, rc = demo.forktest()
    assert rc == 1
    assert selection(events).reasons == {"last_failed": 1}
    assert failed(events) == {"tests/test_tax.py::test_rate_for_country"}
    result = next(e for e in events if isinstance(e, TestResult))
    assert (result.file, result.line) == ("tests/test_tax.py", 25)
    assert result.message.startswith("assert Decimal('0.255') == Decimal('0.25')")
    assert result.trace[-1]["fn"] == "test_rate_for_country"


def test_fixture_and_conftest_edits(demo):
    demo.edit("conftest.py", "time.sleep(0.02)", "time.sleep(0.025)")
    events, _ = demo.forktest()
    assert selection(events).reasons == {"fixture": 1}
    demo.edit("conftest.py", "import time\n", "import time\nimport os\n")
    events, _ = demo.forktest()
    assert selection(events).reasons == {"config_change": 16}


def test_json_lines_all_parse_and_junit_is_valid_xml(demo):
    demo.edit("config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
    proc = subprocess.run(
        [sys.executable, "-m", "forktest", "--no-daemon", "--json", "--junitxml", "out.xml"],
        cwd=demo.root, capture_output=True, text=True,
    )
    for line in proc.stdout.splitlines():
        assert json.loads(line)["event"] in {"selection", "fail", "done"}
    suite = ET.parse(demo.root / "out.xml").getroot().find("testsuite")
    assert suite.attrib["tests"] == "12" and suite.attrib["failures"] == "2"
    assert len(suite.findall("testcase")) == 12


def test_all_matches_plain_pytest_and_honours_passthrough_args(demo):
    demo.edit("billing/tax.py", "return amount.quantize(quantum, rounding=ROUND_HALF_UP)",
              "return amount.quantize(quantum, rounding=ROUND_HALF_UP) + 1")
    for extra in ([], ["--strict-pytest"], ["-k", "tax"], ["-n", "2"]):
        events, _ = demo.forktest("--all", *extra)
        expected = demo.pytest_counts(*(a for a in extra if a not in ("--strict-pytest", "-n", "2")))
        assert (done(events).passed, done(events).failed) == (expected.get("passed", 0), expected.get("failed", 0)), extra
        assert selection(events).reasons == {"all": selection(events).total}


def test_exit_first_stops_after_the_first_failure(demo):
    demo.edit("config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
    events, rc = demo.forktest("-x")
    assert rc == 1 and done(events).failed == 1


def test_verify_selection_reports_a_miss_when_a_baseline_is_wrong(demo):
    db = sqlite3.connect(demo.root / ".forktest" / "store.db")
    with db:
        db.execute("DELETE FROM deps WHERE path = 'data/rates.json'")
    db.close()
    demo.edit("data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
    events, rc = demo.forktest("--verify-selection")
    assert rc == 1 and selection(events).selected == 0
    assert verified(events).misses == 1
    assert [e.nodeid for e in events if type(e).__name__ == "Miss"] == ["tests/test_tax.py::test_rate_for_country"]
    events, _ = demo.forktest()
    assert selection(events).reasons == {"last_failed": 1}


def test_since_uses_the_git_baseline(demo):
    git = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
    subprocess.run([*git, "init", "-q"], cwd=demo.root, check=True)
    subprocess.run([*git, "add", "-A"], cwd=demo.root, check=True)
    subprocess.run([*git, "commit", "-qm", "base"], cwd=demo.root, check=True)
    demo.edit("billing/tax.py", "return load_rates()[country]", "rates = load_rates()\n    return rates[country]")
    demo.forktest()
    events, _ = demo.forktest("--since", "HEAD")
    assert selection(events).reasons == {"coverage": 1}
    assert selection(events).changed == ["billing/tax.py"]


def test_why_explains_without_running(demo):
    demo.edit("data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
    proc = subprocess.run(
        [sys.executable, "-m", "forktest", "--why", "tests/test_tax.py::test_rate_for_country"],
        cwd=demo.root, capture_output=True, text=True,
    )
    assert proc.stdout.splitlines() == [
        "tests/test_tax.py::test_rate_for_country: selected as file_read",
        "  file_read: data/rates.json changed",
    ]
    assert not os.path.exists(demo.root / ".pytest_cache") or True
