from forktest.store import Dep, Record, Store, StoreView, TestRow


def test_record_then_view_roundtrip_and_replace_only_tests_that_ran(tmp_path):
    store = Store(str(tmp_path / ".forktest" / "store.db"))
    a_deps = [Dep("a.py", "f", "h1"), Dep("data.json", "<file>", "s1")]
    b_deps = [Dep("b.py", "g", "h2")]
    store.record([Record("t.py::a", "passed", 0.1, a_deps), Record("t.py::b", "failed", 0.2, b_deps)])
    view = store.view()
    assert view.tests["t.py::a"].outcome == "passed"
    assert view.tests["t.py::b"] == TestRow("failed", 0.2, view.tests["t.py::b"].last_run)
    assert sorted(view.deps["t.py::a"]) == sorted(a_deps)
    assert view.deps["t.py::b"] == b_deps

    store.record([Record("t.py::a", "passed", 0.3, [Dep("a.py", "f", "h1-new")])])
    view = store.view()
    assert view.deps["t.py::a"] == [Dep("a.py", "f", "h1-new")]
    assert view.deps["t.py::b"] == b_deps
    assert view.tests["t.py::a"].duration == 0.3
    store.close()

    reopened = Store(str(tmp_path / ".forktest" / "store.db"))
    assert set(reopened.view().tests) == {"t.py::a", "t.py::b"}


def test_meta_roundtrip(tmp_path):
    store = Store(str(tmp_path / "store.db"))
    assert store.meta_get("preimport") is None
    store.meta_set("preimport", '["json"]')
    store.meta_set("preimport", '["json", "re"]')
    assert store.meta_get("preimport") == '["json", "re"]'


def test_rebased_view_replaces_hashes_and_marks_missing_as_changed():
    view = StoreView({"t::x": TestRow("passed", 0, 0)}, {"t::x": [Dep("a.py", "f", "cur"), Dep("b.py", "g", "cur")]})
    rebased = view.rebased(lambda path, q: "old" if path == "a.py" else None)
    assert rebased.deps["t::x"] == [Dep("a.py", "f", "old"), Dep("b.py", "g", "")]
    assert view.deps["t::x"][0].hash == "cur"
