"""tests/test_cli_demo.py against the Rust binary, plus partial collection and the drop-in passthrough."""

import json
import os
import re
import sqlite3
import subprocess
import xml.etree.ElementTree as ET

from conftest import Miss, TestResult, Verified, done, failed, selection, verified

ROUND_VAT_EXECUTORS = {
    "tests/test_tax.py::test_round_vat[10.005-10.01]",
    "tests/test_tax.py::test_round_vat[10.004-10.00]",
    "tests/test_tax.py::test_round_vat[0.125-0.13]",
    "tests/test_tax.py::test_vat_for_default_rate",
    "tests/test_tax.py::test_vat_for_explicit_rate",
    "tests/test_invoice.py::TestInvoice::test_total_includes_vat",
    "tests/test_invoice.py::TestInvoice::test_describe_currency",
}
ROUND = "return amount.quantize(quantum, rounding=ROUND_HALF_UP)"


def db(demo):
    return sqlite3.connect(demo.root / ".forktest" / "forktest.db")


def stored(demo, pattern: str = "%") -> set[str]:
    conn = db(demo)
    try:
        return {n for (n,) in conn.execute("SELECT nodeid FROM tests WHERE nodeid LIKE ?", (pattern,))}
    finally:
        conn.close()


def test_first_run_selects_all_and_a_rerun_selects_nothing(demo):
    events, rc = demo.forktest("--verify-selection")
    assert rc == 0
    assert (selection(events).selected, selection(events).total) == (0, 16)
    assert done(events).passed == 0
    assert verified(events) == Verified(16, 0, verified(events).duration_ms)


def test_function_body_edit_selects_exactly_the_tests_that_execute_it(demo):
    demo.edit("billing/tax.py", ROUND, ROUND + " + 1")
    events, rc = demo.forktest("--verify-selection")
    assert rc == 1
    assert selection(events).changed == ["billing/tax.py"]
    assert selection(events).reasons == {"coverage": len(ROUND_VAT_EXECUTORS)}
    assert failed(events) == ROUND_VAT_EXECUTORS
    assert verified(events).misses == 0 and verified(events).checked == 16 - len(ROUND_VAT_EXECUTORS)
    assert demo.pytest_counts() == {"failed": 7, "passed": 9}


def test_full_runs_update_every_row_and_re_record_only_tests_whose_deps_changed(demo):
    def last_runs():
        conn = db(demo)
        try:
            return dict(conn.execute("SELECT nodeid, last_run FROM tests"))
        finally:
            conn.close()

    before = last_runs()
    demo.edit("billing/tax.py", ROUND, ROUND + " + 0")
    for args in (("--all",), ("--all", "--strict-pytest")):
        events, rc = demo.forktest(*args)
        assert rc == 0 and done(events).passed == 16, args
        after = last_runs()
        assert after.keys() == before.keys() and all(after[n] > before[n] for n in before), args
        before = after
    events, _ = demo.forktest()
    assert selection(events).selected == 0
    demo.edit("billing/tax.py", ROUND + " + 0", ROUND)
    events, _ = demo.forktest()
    assert selection(events).reasons == {"coverage": len(ROUND_VAT_EXECUTORS)}


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
    proc = demo.run("--no-daemon", "--json", "--junitxml", "out.xml")
    for line in proc.stdout.splitlines():
        assert json.loads(line)["event"] in {"selection", "fail", "done"}
    suite = ET.parse(demo.root / "out.xml").getroot().find("testsuite")
    assert suite.attrib["tests"] == "12" and suite.attrib["failures"] == "2"
    assert len(suite.findall("testcase")) == 12


def test_the_schedule_plans_with_what_a_test_cost_and_reports_keep_pytest_durations(demo):
    with open(demo.root / "conftest.py", "a") as f:
        f.write(
            "\n\ndef pytest_runtest_logreport(report):\n"
            "    if report.when == 'call' and report.nodeid.endswith('test_currency'):\n"
            "        time.sleep(0.3)\n"
        )
    proc = demo.run("--no-daemon", "--json", "--junitxml", "out.xml", "--all")
    assert proc.returncode == 0, proc.stderr
    cases = {c.attrib["name"]: c for c in ET.parse(demo.root / "out.xml").getroot().iter("testcase")}
    assert float(cases["test_currency"].attrib["time"]) < 0.3
    conn = db(demo)
    planned = conn.execute("SELECT cost FROM tests WHERE nodeid = 'tests/test_config.py::test_currency'").fetchone()[0]
    conn.close()
    assert planned >= 0.3


def test_all_matches_plain_pytest_and_honours_passthrough_args(demo):
    demo.edit("billing/tax.py", ROUND, ROUND + " + 1")
    for extra in ([], ["--strict-pytest"], ["-k", "tax"], ["-n", "2"]):
        events, _ = demo.forktest("--all", *extra)
        expected = demo.pytest_counts(*(a for a in extra if a not in ("--strict-pytest", "-n", "2")))
        assert (done(events).passed, done(events).failed) == (expected.get("passed", 0), expected.get("failed", 0)), extra
        assert selection(events).reasons == {"all": selection(events).total}


def test_exit_first_stops_scheduling_after_the_first_failure(demo):
    """Workers already running may still report their failure, so 1 or 2 failures; never the full 12."""
    demo.edit("config.py", 'CURRENCY = "EUR"', 'CURRENCY = "EURO"')
    events, rc = demo.forktest("-x")
    assert rc == 1 and 1 <= done(events).failed <= 2
    assert done(events).passed + done(events).failed < 12


def test_verify_selection_reports_a_miss_when_a_baseline_is_wrong(demo):
    conn = db(demo)
    with conn:
        conn.execute("DELETE FROM key_tests WHERE key_id IN (SELECT id FROM dep_keys WHERE path = 'data/rates.json')")
    conn.close()
    demo.edit("data/rates.json", '"SE": "0.25"', '"SE": "0.255"')
    events, rc = demo.forktest("--verify-selection")
    assert rc == 1 and selection(events).selected == 0
    assert verified(events).misses == 1
    assert [e.nodeid for e in events if isinstance(e, Miss)] == ["tests/test_tax.py::test_rate_for_country"]
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
    proc = demo.run("--why", "tests/test_tax.py::test_rate_for_country", FORKTEST_PYTHON="/nonexistent/python")
    assert proc.stdout.splitlines() == [
        "tests/test_tax.py::test_rate_for_country: selected as file_read",
        "  file_read: data/rates.json changed",
    ]


def test_xfail_and_xpass_are_reported_like_pytest_and_a_strict_xpass_fails(demo):
    demo.write(
        "tests/test_marks.py",
        "import pytest\n\n"
        "@pytest.mark.xfail(reason='known bug')\n"
        "def test_known_bug():\n    assert False\n\n"
        "@pytest.mark.xfail(reason='fixed upstream?')\n"
        "def test_maybe_fixed():\n    assert True\n\n"
        "@pytest.mark.xfail(reason='must fail', strict=True)\n"
        "def test_strict():\n    assert True\n",
    )
    assert demo.pytest_counts() == {"failed": 1, "passed": 16, "xfailed": 1, "xpassed": 1}
    events, rc = demo.forktest("--junitxml", "out.xml")
    assert rc == 1
    assert (done(events).failed, done(events).passed, done(events).xfailed, done(events).xpassed) == (1, 0, 1, 1)
    outcomes = {e.nodeid.split("::")[1]: e.outcome for e in events if isinstance(e, TestResult)}
    assert outcomes == {"test_known_bug": "xfailed", "test_maybe_fixed": "xpassed", "test_strict": "failed"}

    cases = {c.attrib["name"]: c for c in ET.parse(demo.root / "out.xml").getroot().iter("testcase")}
    assert cases["test_known_bug"].find("skipped").attrib == {"type": "pytest.xfail", "message": "known bug"}
    assert list(cases["test_maybe_fixed"]) == [] and cases["test_strict"].find("failure") is not None

    demo.edit("tests/test_marks.py", "assert False", "assert 0 == 1")
    human = demo.run("--no-daemon").stdout
    assert human.splitlines()[-1].startswith("1 failed, 0 passed, 1 xfailed in ")


def test_a_reader_that_closes_early_gets_no_internal_error_and_the_run_still_records(demo):
    demo.edit("billing/tax.py", "ROUND_HALF_UP", "ROUND_HALF_EVEN")
    with subprocess.Popen(
        [demo.binary, "--no-daemon", "--json"],
        cwd=demo.root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=demo.env(),
    ) as proc:
        proc.stdout.readline()
        proc.stdout.close()
        stderr = proc.stderr.read().decode()
        proc.wait(timeout=120)
    assert "INTERNALERROR" not in stderr and "forktest: error" not in stderr and "panicked" not in stderr, stderr
    events, _ = demo.forktest()
    assert set(selection(events).reasons) <= {"last_failed"}


def test_a_module_without_python_source_is_tracked_by_its_content(demo):
    """pandas' editable build imports compiled extensions from inside the project; they have no source to fingerprint."""
    original = (demo.root / "config.py").read_text()

    def compile_config(source: str) -> None:
        (demo.root / "config.py").write_text(source)
        subprocess.run(
            [demo.python, "-c", "import py_compile, sys; py_compile.compile(sys.argv[1], cfile=sys.argv[2])",
             str(demo.root / "config.py"), str(demo.root / "config.pyc")],
            check=True,
        )
        (demo.root / "config.py").unlink()

    compile_config(original)
    events, rc = demo.forktest()
    assert rc == 0 and done(events).failed == 0
    events, rc = demo.forktest()
    assert rc == 0 and selection(events).selected == 0

    compile_config(original.replace('CURRENCY = "EUR"', 'CURRENCY = "EURO"'))
    events, rc = demo.forktest()
    assert selection(events).reasons.get("import_graph", 0) > 0 and done(events).failed > 0


def test_plugins_that_wrap_the_run_protocol_still_apply(demo):
    """pytest applies filterwarnings marks inside a pytest_runtest_protocol wrapper; pandas relies on it."""
    pyproject = demo.root / "pyproject.toml"
    pyproject.write_text(pyproject.read_text() + 'filterwarnings = ["error"]\n')
    demo.write(
        "tests/conftest.py",
        "import os\n"
        "import pytest\n\n\n"
        "@pytest.hookimpl(wrapper=True)\n"
        "def pytest_runtest_protocol(item, nextitem):\n"
        "    os.environ['WRAPPED'] = item.name\n"
        "    return (yield)\n",
    )
    demo.write(
        "tests/test_protocol.py",
        "import os\nimport warnings\n\nimport pytest\n\n\n"
        "@pytest.mark.filterwarnings('ignore:noisy')\n"
        "def test_a_mark_silences_its_own_warning():\n"
        "    warnings.warn('noisy', DeprecationWarning)\n\n\n"
        "def test_the_wrapper_ran_around_this_test():\n"
        "    assert os.environ['WRAPPED'] == 'test_the_wrapper_ran_around_this_test'\n",
    )
    assert demo.pytest_counts() == {"passed": 18}
    events, rc = demo.forktest("--all")
    assert rc == 0 and failed(events) == set() and done(events).passed == 18


def test_forktest_adds_no_modules_a_test_could_lean_on(demo, tmp_path):
    """pandas' test_to_records_with_Mapping_type uses email.message without importing it and only passed under forktest."""
    demo.write(
        "tests/test_isolation.py",
        "import os\nimport sys\n\n\n"
        "def test_record_loaded_modules():\n"
        "    watched = ('email.message', 'ssl', 'http.client', 'urllib.request', 'xml.sax')\n"
        "    with open(os.environ['LOADED_OUT'], 'w') as f:\n"
        "        f.write(' '.join(m for m in watched if m in sys.modules))\n",
    )
    loaded = {}
    for runner in ("pytest", "forktest"):
        out = tmp_path / runner
        os.environ["LOADED_OUT"] = str(out)
        try:
            if runner == "pytest":
                assert demo.pytest_counts("tests/test_isolation.py") == {"passed": 1}
            else:
                events, rc = demo.forktest("tests/test_isolation.py")
                assert rc == 0 and failed(events) == set()
        finally:
            del os.environ["LOADED_OUT"]
        loaded[runner] = out.read_text()
    assert loaded["forktest"] == loaded["pytest"]


def test_a_module_skipped_at_import_counts_like_pytest_in_a_full_run(demo):
    """pandas' test_arrow_interface.py skips itself with importorskip when pyarrow is missing."""
    demo.write(
        "tests/test_optional.py",
        "import pytest\n\npytest.importorskip('a_module_that_is_not_installed')\n\n\n"
        "def test_never_collected():\n    pass\n",
    )
    assert demo.pytest_counts() == {"passed": 16, "skipped": 1}
    events, rc = demo.forktest("--all")
    assert rc == 0 and done(events).passed == 16 and done(events).skipped == 1


def test_a_no_op_run_starts_no_python(demo):
    events, rc = demo.forktest(FORKTEST_PYTHON="/nonexistent/python")
    assert rc == 0 and (selection(events).selected, selection(events).total) == (0, 16)
    assert demo.log("no python started") == ["forktest: no python started"]


def test_an_edit_collects_only_the_files_holding_selected_tests(demo):
    demo.edit("billing/tax.py", ROUND, ROUND + " + 1")
    events, rc = demo.forktest()
    assert selection(events).reasons == {"coverage": len(ROUND_VAT_EXECUTORS)}
    assert failed(events) == ROUND_VAT_EXECUTORS
    assert demo.log("collecting") == ["forktest: collecting 2 files: tests/test_invoice.py tests/test_tax.py"]


def test_a_new_test_in_an_existing_file_is_selected_as_new(demo):
    with open(demo.root / "tests" / "test_config.py", "a") as f:
        f.write("\n\ndef test_added():\n    assert 1 + 1 == 2\n")
    events, rc = demo.forktest()
    assert rc == 0 and selection(events).total == 17
    assert selection(events).reasons["new"] == 1 and done(events).passed == selection(events).selected
    assert "tests/test_config.py::test_added" in stored(demo)
    assert demo.log("collecting") == ["forktest: collecting 1 files: tests/test_config.py"]
    events, _ = demo.forktest()
    assert selection(events).selected == 0 and demo.log("no python started")


def test_a_new_test_file_is_collected_and_selected_as_new(demo):
    demo.write("tests/test_extra.py", "def test_extra():\n    pass\n")
    events, rc = demo.forktest()
    assert rc == 0 and selection(events).reasons == {"new": 1} and selection(events).total == 17
    assert done(events).passed == 1 and "tests/test_extra.py::test_extra" in stored(demo)
    assert demo.log("full collection") == ["forktest: full collection: a test file or conftest.py was added or removed"]


def test_tests_another_args_key_deselected_run_as_new(fresh):
    k = fresh.pytest_counts("-k", "tax")
    events, rc = fresh.forktest("-k", "tax")
    assert rc == 0 and selection(events).reasons == {"new": k["passed"]}
    before = stored(fresh)
    events, rc = fresh.forktest()
    assert rc == 0 and selection(events).reasons == {"new": 16 - k["passed"]}
    assert len(before) == k["passed"] and len(stored(fresh)) == 16


def test_a_parametrize_list_read_from_a_data_file_is_collected_again_when_it_changes(demo):
    demo.write("tests/cases.json", "[1, 2]")
    demo.write(
        "tests/test_cases.py",
        "import json\nimport pathlib\n\nimport pytest\n\n"
        "CASES = json.loads((pathlib.Path(__file__).parent / 'cases.json').read_text())\n\n\n"
        "@pytest.mark.parametrize('n', CASES)\n"
        "def test_case(n):\n    assert n > 0\n",
    )
    events, _ = demo.forktest()
    assert selection(events).reasons == {"new": 2}
    demo.write("tests/cases.json", "[1, 3, 22]")
    events, rc = demo.forktest()
    assert rc == 0 and selection(events).reasons == {"new": 2} and selection(events).total == 19
    assert done(events).passed == 2
    assert demo.log("re-collecting") == ["forktest: re-collecting tests/test_cases.py: collection_deps"]
    events, _ = demo.forktest()
    assert selection(events).selected == 0 and demo.log("no python started")
    assert stored(demo, "%test_case[%") == {"tests/test_cases.py::test_case[1]", "tests/test_cases.py::test_case[3]", "tests/test_cases.py::test_case[22]"}


def without_timing(text: str) -> str:
    return re.sub(r" in \d+\.\d+s", " in Xs", text)


def test_modes_forktest_cannot_serve_are_plain_pytest(fresh):
    for args in (["--co", "-q"], ["--collect-only"], ["--markers"], ["--fixtures", "-q"]):
        ours, theirs = fresh.run(*args), fresh.pytest(*args)
        assert (ours.returncode, without_timing(ours.stdout)) == (theirs.returncode, without_timing(theirs.stdout)), args
    ours, theirs = fresh.run("--version"), fresh.pytest("--version")
    assert (ours.stdout, ours.stderr) == (theirs.stdout, theirs.stderr) and "pytest" in ours.stdout + ours.stderr
    assert not (fresh.root / ".forktest").exists()


def test_subcommands_of_later_units_say_so(fresh):
    for cmd in ("watch", "daemon"):
        proc = fresh.run(cmd)
        assert proc.returncode == 2 and proc.stderr == f"forktest: {cmd} is not available yet\n"


def test_collection_errors_stop_the_run_like_pytest(demo):
    demo.write("tests/test_broken.py", "import nothing_here\n\ndef test_x():\n    pass\n")
    events, rc = demo.forktest()
    assert rc == 2 and done(events).errors == 1 and selection(events) is None
    events, rc = demo.forktest("--continue-on-collection-errors")
    assert rc == 1 and done(events).passed == 0 and selection(events).total == 16
