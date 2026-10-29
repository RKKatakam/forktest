import random
import sqlite3

from forktest.fingerprint import FileInfo, ImportStmt, Stat
from forktest.store import Dep, Record, Store


def deps_of(view, nodeid):
    return sorted(view.keys[k] for k in view.key_ids_of(nodeid))


def users(view, dep):
    """The nodeids the key to tests index lists for one dependency."""
    by_id = {row.id: nodeid for nodeid, row in view.tests.items()}
    key_id = next(i for i, d in view.keys.items() if d == dep)
    return {by_id[t] for t in view.tests_using([key_id])[key_id]}


def test_record_then_view_roundtrip_and_replace_only_tests_that_ran(tmp_path):
    store = Store(str(tmp_path / ".forktest" / "store.db"))
    a_deps = [Dep("a.py", "f", "h1"), Dep("data.json", "<file>", "s1")]
    b_deps = [Dep("b.py", "g", "h2")]
    store.record([Record("t.py::a", "passed", 0.1, a_deps), Record("t.py::b", "failed", 0.2, b_deps)])
    view = store.view()
    assert view.tests["t.py::a"].outcome == "passed"
    assert (view.tests["t.py::b"].outcome, view.tests["t.py::b"].duration) == ("failed", 0.2)
    assert deps_of(view, "t.py::a") == sorted(a_deps)
    assert deps_of(view, "t.py::b") == b_deps
    assert view.module_sets[view.tests["t.py::a"].modules] == {"data.json"}

    store.record([Record("t.py::a", "passed", 0.3, [Dep("a.py", "f", "h1-new")])])
    view = store.view()
    assert deps_of(view, "t.py::a") == [Dep("a.py", "f", "h1-new")]
    assert deps_of(view, "t.py::b") == b_deps
    assert view.tests["t.py::a"].duration == 0.3
    assert view.module_sets[view.tests["t.py::a"].modules] == frozenset()
    store.close()

    reopened = Store(str(tmp_path / ".forktest" / "store.db"))
    assert set(reopened.view().tests) == {"t.py::a", "t.py::b"}


def test_the_index_follows_re_records_and_forgotten_tests(tmp_path):
    store = Store(str(tmp_path / "store.db"))
    shared, only_a, only_b = Dep("s.py", "f", "1"), Dep("a.py", "f", "1"), Dep("b.py", "f", "1")
    store.record([Record("t::a", "passed", 0, [shared, only_a]), Record("t::b", "passed", 0, [shared, only_b])])
    view = store.view()
    assert users(view, shared) == {"t::a", "t::b"} and users(view, only_a) == {"t::a"}

    store.record([Record("t::a", "passed", 0, [only_a, only_b])])
    view = store.view()
    assert users(view, shared) == {"t::b"}
    assert users(view, only_b) == {"t::a", "t::b"}

    store.forget(["t::b"])
    view = store.view()
    assert set(view.tests) == {"t::a"}
    assert set(view.keys.values()) == {only_a, only_b}
    assert users(view, only_b) == {"t::a"}


def test_the_index_stays_the_exact_inverse_of_every_test_s_deps(tmp_path):
    rng = random.Random(7)
    store = Store(str(tmp_path / "store.db"))
    universe = [Dep(f"m{k % 9}.py", f"f{k}", str(k % 3)) for k in range(40)]
    model: dict[str, set[Dep]] = {}
    for step in range(60):
        if rng.random() < 0.2 and model:
            gone = rng.sample(sorted(model), k=rng.randint(1, len(model)))
            store.forget(gone)
            for nodeid in gone:
                del model[nodeid]
        else:
            batch = {f"t::{rng.randrange(12)}": set(rng.sample(universe, k=rng.randint(0, 12))) for _ in range(rng.randint(1, 5))}
            store.record(Record(n, "passed", 0, list(ds)) for n, ds in batch.items())
            model.update(batch)
        view = store.view()
        live = set().union(*model.values()) if model else set()
        assert set(view.keys.values()) == live, f"step {step}: dead or missing keys"
        assert set(view.tests) == set(model)
        for dep in live:
            assert users(view, dep) == {n for n, ds in model.items() if dep in ds}, f"step {step}: {dep}"
        for nodeid, ds in model.items():
            assert deps_of(view, nodeid) == sorted(ds)


def test_meta_roundtrip(tmp_path):
    store = Store(str(tmp_path / "store.db"))
    assert store.meta_get("preimport") is None
    store.meta_set("preimport", '["json"]')
    store.meta_set("preimport", '["json", "re"]')
    assert store.meta_get("preimport") == '["json", "re"]'


def test_rebased_view_replaces_hashes_and_marks_missing_as_changed(tmp_path):
    store = Store(str(tmp_path / "store.db"))
    store.record([Record("t::x", "passed", 0, [Dep("a.py", "f", "cur"), Dep("b.py", "g", "cur")])])
    view = store.view()
    rebased = view.rebased(lambda path, q: "old" if path == "a.py" else None)
    assert deps_of(rebased, "t::x") == [Dep("a.py", "f", "old"), Dep("b.py", "g", "")]
    assert deps_of(view, "t::x") == [Dep("a.py", "f", "cur"), Dep("b.py", "g", "cur")]


def test_file_infos_survive_a_reopen_and_replace_by_path(tmp_path):
    path = str(tmp_path / "store.db")
    store = Store(path)
    stat = Stat(1_700_000_000_000_000_001, 1_700_000_000_000_000_002, 42)
    old = FileInfo("s1", {"<module>": "m1", "f": "h1"}, [ImportStmt("a.b", 0, ()), ImportStmt("", 1, ("x", "y"))], stat)
    new = FileInfo("s2", {"<module>": "m2"}, [])
    store.save_file_infos({"m.py": old, "data.json": FileInfo("s3", {"<file>": "s3"}, [])})
    store.save_file_infos({"m.py": new})
    store.close()
    assert Store(path).file_infos() == {"m.py": new, "data.json": FileInfo("s3", {"<file>": "s3"}, [])}
    reopened = Store(path)
    reopened.save_file_infos({"m.py": old})
    assert reopened.file_infos()["m.py"] == old


def test_superseded_dependency_keys_are_deleted(tmp_path):
    path = str(tmp_path / "store.db")
    store = Store(path)
    for version in range(20):
        deps = [Dep(f"m{k}.py", "f", f"v{version}") for k in range(100)] + [Dep("shared.py", "g", "h")]
        store.record([Record("t.py::a", "passed", 0.1, deps), Record("t.py::b", "passed", 0.1, deps[-2:])])
    assert deps_of(store.view(), "t.py::a") == sorted(deps)
    assert deps_of(store.view(), "t.py::b") == sorted(deps[-2:])
    db = sqlite3.connect(path)
    assert db.execute("SELECT count(*) FROM dep_keys").fetchone()[0] == 101
    assert db.execute("SELECT count(*) FROM key_tests").fetchone()[0] == 101
    assert db.execute("SELECT count(*) FROM module_sets").fetchone()[0] == 1


def test_a_store_from_the_previous_layout_starts_empty(tmp_path):
    path = tmp_path / "store.db"
    old = sqlite3.connect(path)
    old.executescript(
        "CREATE TABLE tests(nodeid TEXT PRIMARY KEY, outcome TEXT NOT NULL, duration REAL NOT NULL,"
        " last_run REAL NOT NULL, deps TEXT NOT NULL);"
        "CREATE TABLE dep_keys(id INTEGER PRIMARY KEY, path TEXT, qualname TEXT, hash TEXT, UNIQUE(path, qualname, hash));"
        "CREATE TABLE files(path TEXT PRIMARY KEY, sha TEXT, fingerprint TEXT, imports TEXT);"
        "INSERT INTO dep_keys VALUES (1, 'a.py', 'f', 'h');"
        "INSERT INTO tests VALUES ('t.py::a', 'passed', 0.1, 0, '[1]');"
        "INSERT INTO files VALUES ('a.py', 's', '{}', '[]');"
        "PRAGMA user_version = 2;"
    )
    old.commit()
    old.close()
    store = Store(str(path))
    assert store.view().tests == {} and store.file_infos() == {}
    store.record([Record("t.py::a", "failed", 0.2, [Dep("a.py", "f", "h2")])])
    assert deps_of(store.view(), "t.py::a") == [Dep("a.py", "f", "h2")]
