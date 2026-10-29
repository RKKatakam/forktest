import sqlite3

from forktest.fingerprint import FileInfo, ImportStmt
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


def test_file_infos_survive_a_reopen_and_replace_by_path(tmp_path):
    path = str(tmp_path / "store.db")
    store = Store(path)
    old = FileInfo("s1", {"<module>": "m1", "f": "h1"}, [ImportStmt("a.b", 0, ()), ImportStmt("", 1, ("x", "y"))])
    new = FileInfo("s2", {"<module>": "m2"}, [])
    store.save_file_infos({"m.py": old, "data.json": FileInfo("s3", {"<file>": "s3"}, [])})
    store.save_file_infos({"m.py": new})
    store.close()
    assert Store(path).file_infos() == {"m.py": new, "data.json": FileInfo("s3", {"<file>": "s3"}, [])}
    reopened = Store(path)
    reopened.save_file_infos({"m.py": old})
    assert reopened.file_infos()["m.py"] == old


def test_superseded_dependency_keys_are_collected(tmp_path):
    path = str(tmp_path / "store.db")
    store = Store(path)
    for version in range(20):
        deps = [Dep(f"m{k}.py", "f", f"v{version}") for k in range(100)] + [Dep("shared.py", "g", "h")]
        store.record([Record("t.py::a", "passed", 0.1, deps), Record("t.py::b", "passed", 0.1, deps[-2:])])
    assert sorted(store.view().deps["t.py::a"]) == sorted(deps)
    assert store.view().deps["t.py::b"] == sorted(deps[-2:])
    keys = sqlite3.connect(path).execute("SELECT count(*) FROM dep_keys").fetchone()[0]
    assert keys <= 2 * 101 + 256


def test_a_store_from_the_old_layout_starts_empty(tmp_path):
    path = tmp_path / "store.db"
    old = sqlite3.connect(path)
    old.executescript(
        "CREATE TABLE tests(nodeid TEXT PRIMARY KEY, outcome TEXT, duration REAL, last_run REAL);"
        "CREATE TABLE deps(nodeid TEXT, path TEXT, qualname TEXT, hash TEXT);"
        "INSERT INTO tests VALUES ('t.py::a', 'passed', 0.1, 0); INSERT INTO deps VALUES ('t.py::a', 'a.py', 'f', 'h');"
    )
    old.commit()
    old.close()
    store = Store(str(path))
    assert store.view().tests == {}
    store.record([Record("t.py::a", "failed", 0.2, [Dep("a.py", "f", "h2")])])
    assert store.view().deps == {"t.py::a": [Dep("a.py", "f", "h2")]}
