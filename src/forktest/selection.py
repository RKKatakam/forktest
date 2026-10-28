"""Which tests a change affects, and why. Pure functions over a store view."""

from __future__ import annotations

import enum
import fnmatch
import os
import posixpath
from collections.abc import Callable, Iterable, Mapping

from forktest.fingerprint import FILE, MODULE, Fingerprints, ImportStmt, normalize_qualname
from forktest.store import Dep, StoreView

FingerprintFn = Callable[[str], "dict[str, str] | None"]


class Reason(enum.StrEnum):
    NEW = "new"
    COVERAGE = "coverage"
    FIXTURE = "fixture"
    IMPORT_GRAPH = "import_graph"
    FILE_READ = "file_read"
    LAST_FAILED = "last_failed"
    CONFIG_CHANGE = "config_change"
    ALL = "all"


PRIORITY = [
    Reason.ALL,
    Reason.CONFIG_CHANGE,
    Reason.NEW,
    Reason.LAST_FAILED,
    Reason.COVERAGE,
    Reason.FIXTURE,
    Reason.FILE_READ,
    Reason.IMPORT_GRAPH,
]

CONFIG_FILES = ("pytest.ini", ".pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini", "uv.lock", "poetry.lock")
CONFIG_GLOBS = ("requirements*.txt",)
EXCLUDED_DIRS = frozenset(
    {".git", ".hg", ".venv", "venv", ".tox", ".nox", "__pycache__", ".forktest", ".pytest_cache", "node_modules", "site-packages"}
)


def is_config_file(path: str) -> bool:
    name = posixpath.basename(path)
    return name in CONFIG_FILES or any(fnmatch.fnmatch(name, g) for g in CONFIG_GLOBS)


def config_files(root: str) -> list[str]:
    return [name for name in sorted(os.listdir(root)) if is_config_file(name) and os.path.isfile(os.path.join(root, name))]


def is_excluded(relpath: str) -> bool:
    return relpath.startswith("..") or any(part in EXCLUDED_DIRS for part in relpath.split("/"))


class ImportGraph:
    """Static import edges between project files.

    A module name resolves through ``known_modules`` (a name to project-path
    map, usually taken from ``sys.modules``) first, then through the file
    system under the project root, ``src/``, and the importing file's own
    top-level directory, which is where pytest's default import mode puts a
    test directory without ``__init__.py``.
    """

    def __init__(self, root: str, fingerprints: Fingerprints, known_modules: Mapping[str, str] | None = None) -> None:
        self.root = root
        self.fps = fingerprints
        self.known = known_modules or {}
        self._direct: dict[str, tuple[str | None, frozenset[str]]] = {}

    def _exists(self, relpath: str) -> bool:
        return not is_excluded(relpath) and os.path.isfile(os.path.join(self.root, relpath))

    def _basedir(self, importer: str) -> str:
        d = posixpath.dirname(importer)
        while d and os.path.isfile(os.path.join(self.root, d, "__init__.py")):
            d = posixpath.dirname(d)
        return d

    def _files_for(self, dotted: str, search_dirs: Iterable[str]) -> set[str]:
        out: set[str] = set()
        rel = dotted.replace(".", "/")
        for d in search_dirs:
            for cand in (posixpath.join(d, rel + ".py"), posixpath.join(d, rel, "__init__.py")):
                if self._exists(cand):
                    out.add(cand)
        return out

    def resolve(self, stmt: ImportStmt, importer: str) -> set[str]:
        out: set[str] = set()
        if stmt.level:
            base = posixpath.dirname(importer)
            for _ in range(stmt.level - 1):
                base = posixpath.dirname(base)
            targets = [stmt.module] if stmt.module else []
            targets += [f"{stmt.module}.{n}" if stmt.module else n for n in stmt.names]
            if stmt.module:
                parts = stmt.module.split(".")
                targets += [".".join(parts[:i]) for i in range(1, len(parts))]
            for t in targets:
                out |= self._files_for(t, [base])
            if self._exists(posixpath.join(base, "__init__.py")):
                out.add(posixpath.join(base, "__init__.py"))
            return out
        targets = [stmt.module] + [f"{stmt.module}.{n}" for n in stmt.names]
        parts = stmt.module.split(".")
        targets += [".".join(parts[:i]) for i in range(1, len(parts))]
        search_dirs = ["", "src", self._basedir(importer)]
        for t in targets:
            known = self.known.get(t)
            if known is not None and self._exists(known):
                out.add(known)
            else:
                out |= self._files_for(t, search_dirs)
        return out

    def direct(self, path: str) -> frozenset[str]:
        sha = self.fps.sha(path)
        cached = self._direct.get(path)
        if cached and cached[0] == sha:
            return cached[1]
        edges: set[str] = set()
        if sha is not None and path.endswith(".py"):
            for stmt in self.fps.imports_of(path):
                edges |= self.resolve(stmt, path)
        edges.discard(path)
        result = frozenset(edges)
        self._direct[path] = (sha, result)
        return result

    def conftests_for(self, path: str) -> list[str]:
        out = []
        d = posixpath.dirname(path)
        parts = d.split("/") if d else []
        for i in range(len(parts) + 1):
            cand = posixpath.join(*parts[:i], "conftest.py") if i else "conftest.py"
            if self._exists(cand):
                out.append(cand)
        return out

    def closure(self, path: str) -> set[str]:
        """Every project file the test file's execution can touch at import time."""
        seen: set[str] = set()
        stack = [path, *self.conftests_for(path)]
        while stack:
            p = stack.pop()
            if p in seen or not self._exists(p):
                continue
            seen.add(p)
            stack.extend(self.direct(p))
        return seen


def dep_changed(dep: Dep, current: FingerprintFn) -> bool:
    fp = current(dep.path)
    if fp is None:
        return True
    qualname = dep.qualname if dep.qualname in fp else normalize_qualname(dep.qualname, fp)
    return fp.get(qualname) != dep.hash


def classify(dep: Dep, fixtures: frozenset[tuple[str, str]]) -> Reason:
    name = posixpath.basename(dep.path)
    if dep.qualname == FILE:
        return Reason.CONFIG_CHANGE if is_config_file(name) else Reason.FILE_READ
    if dep.qualname == MODULE:
        return Reason.CONFIG_CHANGE if name == "conftest.py" else Reason.IMPORT_GRAPH
    return Reason.FIXTURE if (dep.path, dep.qualname) in fixtures else Reason.COVERAGE


def reasons_for(
    nodeid: str,
    view: StoreView,
    current: FingerprintFn,
    graph: ImportGraph,
    fixtures: frozenset[tuple[str, str]],
    closure_cache: dict[str, set[str]],
) -> list[tuple[Reason, Dep | None]]:
    row = view.tests.get(nodeid)
    if row is None:
        return [(Reason.NEW, None)]
    out: list[tuple[Reason, Dep | None]] = []
    if row.outcome in ("failed", "error"):
        out.append((Reason.LAST_FAILED, None))
    deps = view.deps.get(nodeid, [])
    out.extend((classify(d, fixtures), d) for d in deps if dep_changed(d, current))
    test_file = nodeid.split("::")[0]
    if test_file not in closure_cache:
        closure_cache[test_file] = graph.closure(test_file)
    recorded_modules = {d.path for d in deps if d.qualname == MODULE}
    out.extend(
        (Reason.IMPORT_GRAPH, Dep(p, MODULE, "")) for p in sorted(closure_cache[test_file]) if p not in recorded_modules
    )
    return out


def select(
    nodeids: Iterable[str],
    view: StoreView,
    current: FingerprintFn,
    graph: ImportGraph,
    *,
    fixtures: frozenset[tuple[str, str]] = frozenset(),
) -> dict[str, Reason]:
    """Map each affected nodeid to the highest-priority reason it is selected."""
    closures: dict[str, set[str]] = {}
    out: dict[str, Reason] = {}
    for nodeid in nodeids:
        reasons = reasons_for(nodeid, view, current, graph, fixtures, closures)
        if reasons:
            out[nodeid] = min((r for r, _ in reasons), key=PRIORITY.index)
    return out


def changed_paths(nodeids: Iterable[str], view: StoreView, current: FingerprintFn) -> list[str]:
    paths = {d.path for n in nodeids for d in view.deps.get(n, []) if dep_changed(d, current)}
    return sorted(paths)
