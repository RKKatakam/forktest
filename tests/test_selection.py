import pytest

from forktest.fingerprint import FILE, MODULE, Fingerprints
from forktest.selection import ImportGraph, Reason, changed_paths, config_files, is_config_file, select
from forktest.store import Dep, StoreView, TestRow

FILES = {
    "pyproject.toml": "[tool.pytest.ini_options]\n",
    "conftest.py": "import helpers\n\ndef fx():\n    return helpers.h()\n",
    "helpers.py": "def h():\n    return 1\n",
    "config.py": "X = 1\n",
    "pkg/__init__.py": "",
    "pkg/core.py": "from pkg import util\nfrom config import X\n\ndef f():\n    return util.g() + X\n",
    "pkg/util.py": "def g():\n    return 2\n",
    "src/lib/__init__.py": "",
    "src/lib/thing.py": "def t():\n    return 3\n",
    "tests/conftest.py": "",
    "tests/test_core.py": "from pkg.core import f\nimport lib.thing\n\ndef test_f():\n    assert f()\n",
    "tests/sub/local_helper.py": "def lh():\n    return 4\n",
    "tests/sub/test_deep.py": "import local_helper\nimport newmod\n\ndef test_deep():\n    assert local_helper.lh()\n",
    "data.json": "{}",
}


@pytest.fixture
def project(tmp_path):
    for rel, text in FILES.items():
        p = tmp_path / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text)
    return tmp_path


def graph_for(root):
    return ImportGraph(str(root), Fingerprints(str(root)))


def test_closure_follows_absolute_relative_src_and_conftest_imports(project):
    graph = graph_for(project)
    assert graph.closure("tests/test_core.py") == {
        "tests/test_core.py",
        "pkg/core.py",
        "pkg/__init__.py",
        "pkg/util.py",
        "config.py",
        "src/lib/thing.py",
        "src/lib/__init__.py",
        "conftest.py",
        "helpers.py",
        "tests/conftest.py",
    }


def test_closure_resolves_through_the_test_directory_and_known_modules(project):
    graph = graph_for(project)
    closure = graph.closure("tests/sub/test_deep.py")
    assert "tests/sub/local_helper.py" in closure
    assert "conftest.py" in closure and "tests/conftest.py" in closure
    assert not any(p.startswith("pkg/") for p in closure)
    known = ImportGraph(str(project), Fingerprints(str(project)), {"newmod": "src/lib/thing.py"})
    assert "src/lib/thing.py" in known.closure("tests/sub/test_deep.py")


def test_config_file_detection(project):
    assert is_config_file("pyproject.toml") and is_config_file("requirements-dev.txt")
    assert not is_config_file("tests/conftest.py")
    assert config_files(str(project)) == ["pyproject.toml"]


NODE = "tests/test_core.py::test_f"
DEEP = "tests/sub/test_deep.py::test_deep"


def recorded_view(root, outcome="passed"):
    """A baseline in which every dep hash matches the files as they are now."""
    fps = Fingerprints(str(root))
    graph = graph_for(root)

    def dep(path, qualname):
        return Dep(path, qualname, fps.of(path)[qualname])

    deps = [dep("pkg/core.py", "f"), dep("pkg/util.py", "g"), dep("conftest.py", "fx"), dep("data.json", FILE)]
    deps += [dep(p, MODULE) for p in graph.closure("tests/test_core.py")]
    deps += [dep("pyproject.toml", FILE)]
    deep = [dep(p, MODULE) for p in graph.closure("tests/sub/test_deep.py")]
    return StoreView(
        {NODE: TestRow(outcome, 0.1, 0), DEEP: TestRow("passed", 0.1, 0)},
        {NODE: deps, DEEP: deep},
    )


def run_select(root, view, fixtures=frozenset()):
    fps = Fingerprints(str(root))
    graph = ImportGraph(str(root), fps)
    return select([NODE, DEEP, "tests/test_core.py::test_new"], view, fps.of, graph, fixtures=fixtures)


def edit(root, rel, old, new):
    p = root / rel
    p.write_text(p.read_text().replace(old, new))


def test_unchanged_selects_only_new_tests(project):
    assert run_select(project, recorded_view(project)) == {"tests/test_core.py::test_new": Reason.NEW}


def test_last_failed_reruns(project):
    assert run_select(project, recorded_view(project, outcome="failed"))[NODE] == Reason.LAST_FAILED


@pytest.mark.parametrize(
    ("rel", "old", "new", "reason"),
    [
        ("pkg/util.py", "return 2", "return 20", Reason.COVERAGE),
        ("conftest.py", "helpers.h()", "helpers.h() + 1", Reason.FIXTURE),
        ("data.json", "{}", '{"k": 1}', Reason.FILE_READ),
        ("config.py", "X = 1", "X = 2", Reason.IMPORT_GRAPH),
        ("src/lib/thing.py", "def t():", "def t(a=1):", Reason.IMPORT_GRAPH),
        ("conftest.py", "import helpers", "import helpers, os", Reason.CONFIG_CHANGE),
        ("pyproject.toml", "ini_options", "ini_options2", Reason.CONFIG_CHANGE),
    ],
)
def test_each_kind_of_change_selects_with_its_reason(project, rel, old, new, reason):
    view = recorded_view(project)
    edit(project, rel, old, new)
    result = run_select(project, view, fixtures=frozenset({("conftest.py", "fx")}))
    assert result[NODE] == reason
    assert changed_paths([NODE], view, Fingerprints(str(project)).of) == [rel]


def test_docstring_and_unrelated_body_edits_select_nothing(project):
    view = recorded_view(project)
    edit(project, "pkg/util.py", "def g():\n", 'def g():\n    """Doc."""\n')
    edit(project, "tests/sub/local_helper.py", "return 4", "return 40")
    assert NODE not in run_select(project, view)


def test_removed_function_and_deleted_file_count_as_changed(project):
    view = recorded_view(project)
    (project / "data.json").unlink()
    assert run_select(project, view)[NODE] == Reason.FILE_READ
    (project / "data.json").write_text("{}")
    view = recorded_view(project)
    edit(project, "pkg/util.py", "def g():", "def g2():")
    assert run_select(project, view)[NODE] == Reason.COVERAGE


def test_raw_lambda_qualname_is_normalized_before_comparison(project):
    fps = Fingerprints(str(project))
    view = recorded_view(project)
    view.deps[NODE].append(Dep("pkg/util.py", "g.<locals>.<lambda>", fps.of("pkg/util.py")["g"]))
    assert NODE not in run_select(project, view)


def test_newly_created_module_that_an_import_now_resolves_to_selects(project):
    view = recorded_view(project)
    (project / "tests/sub/newmod.py").write_text("Y = 1\n")
    assert run_select(project, view)[DEEP] == Reason.IMPORT_GRAPH


def test_priority_puts_config_before_last_failed_before_coverage(project):
    view = recorded_view(project, outcome="failed")
    edit(project, "pkg/util.py", "return 2", "return 20")
    assert run_select(project, view)[NODE] == Reason.LAST_FAILED
    edit(project, "pyproject.toml", "ini_options", "ini_options2")
    assert run_select(project, view)[NODE] == Reason.CONFIG_CHANGE


def test_since_rebased_baseline(project):
    view = recorded_view(project)
    fps = Fingerprints(str(project))
    old_core = Fingerprints(str(project))
    rebased = view.rebased(lambda path, q: "older" if (path, q) == ("pkg/util.py", "g") else (fps.of(path) or {}).get(q))
    assert run_select(project, rebased)[NODE] == Reason.COVERAGE
    assert old_core.of("pkg/util.py")["g"] == fps.of("pkg/util.py")["g"]
