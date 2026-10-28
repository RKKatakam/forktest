import io
import xml.etree.ElementTree as ET

import pytest

from forktest.events import CollectError, Done, Miss, Selection, TestResult, Verified, dumps, parse
from forktest.reporters import HumanReporter, JsonlReporter, JunitReporter

EVENTS = [
    Selection(["billing/tax.py"], 2, 16, {"coverage": 2}, 40),
    TestResult("tests/test_tax.py::test_a", "passed", 0.01),
    TestResult(
        "tests/test_tax.py::test_vat_rounding",
        "failed",
        0.002,
        file="billing/tax.py",
        line=42,
        message="assert Decimal('10.00') == Decimal('10.01')\n +  where ...",
        trace=[{"file": "billing/tax.py", "line": 42, "fn": "round_vat"}],
    ),
    TestResult("tests/test_x.py::test_s", "skipped", 0.0, message="Skipped: no"),
    CollectError("tests/test_broken.py", "ImportError: nope"),
    Miss("tests/test_y.py::test_m", "assert False"),
    Verified(14, 1, 231),
    Done(13, 1, 0, 1, 231),
]


@pytest.mark.parametrize("event", EVENTS, ids=lambda e: type(e).__name__)
def test_json_roundtrip(event):
    assert parse(dumps(event)) == event


def test_jsonl_matches_design_shape_and_omits_passes():
    out = io.StringIO()
    rep = JsonlReporter(out)
    for e in EVENTS:
        rep.handle(e)
    lines = out.getvalue().splitlines()
    kinds = [parse(line).__class__.__name__ for line in lines]
    assert '"event":"pass"' not in out.getvalue()
    assert kinds == ["Selection", "TestResult", "TestResult", "CollectError", "Miss", "Verified", "Done"]
    assert '"event":"fail","test":"tests/test_tax.py::test_vat_rounding"' in lines[1]
    assert lines[0].startswith('{"event":"selection","changed":["billing/tax.py"],"selected":2,"total":16')


def test_human_output_streams_failures_and_summarizes_files():
    out = io.StringIO()
    rep = HumanReporter(out, expect_verify=True)
    for e in EVENTS:
        rep.handle(e)
    text = out.getvalue()
    assert text.splitlines()[0] == "forktest · 1 file changed · 2 of 16 tests selected (0.04s)"
    assert "✗ tests/test_tax.py::test_vat_rounding" in text
    assert "billing/tax.py:42  assert Decimal('10.00') == Decimal('10.01')" in text
    assert "✓ tests/test_x.py" in text and "1 skipped" in text
    assert "MISS tests/test_y.py::test_m" in text
    assert "1 failed, 13 passed, 1 skipped in 0.23s · safety net" in text
    assert text.index("✗ tests/test_tax.py") < text.index("✓ tests/test_x.py")


def test_junit_is_valid_xml_with_counts(tmp_path):
    path = tmp_path / "junit.xml"
    rep = JunitReporter(str(path))
    for e in EVENTS:
        rep.handle(e)
    rep.close()
    suite = ET.parse(path).getroot().find("testsuite")
    assert suite.attrib["tests"] == "3"
    assert suite.attrib["failures"] == "1"
    assert suite.attrib["errors"] == "1"
    cases = suite.findall("testcase")
    assert [c.attrib["name"] for c in cases] == ["test_a", "test_vat_rounding", "test_s", "tests/test_broken.py"]
    assert cases[1].find("failure").attrib["message"].startswith("assert Decimal")
