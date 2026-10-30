"""A run that runs everything reports the counts plain pytest reports."""

from conftest import done

SKIPPED_MODULE = 'import pytest\n\npytest.importorskip("no_such_module_anywhere")\n\n\ndef test_never():\n    pass\n'


def test_a_first_run_counts_a_module_level_skip_as_pytest_does(fresh):
    fresh.write("tests/test_optional.py", SKIPPED_MODULE)
    events, rc = fresh.forktest()
    assert rc == 0
    counts = done(events)
    assert {"passed": counts.passed, "skipped": counts.skipped} == fresh.pytest_counts()


def test_a_partial_run_does_not_repeat_skips_from_files_it_did_not_run(fresh):
    fresh.write("tests/test_optional.py", SKIPPED_MODULE)
    fresh.forktest()
    fresh.edit("billing/tax.py", "def vat_for(", "def vat_for(  ")
    events, _ = fresh.forktest()
    assert done(events).skipped == 0
