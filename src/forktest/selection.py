"""Which tests a change affects, and why. Pure functions over a store view."""

from __future__ import annotations

import enum
import fnmatch
import importlib.machinery
import os
import posixpath
from collections.abc import Callable, Iterable, Mapping
from typing import NamedTuple

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
    {
        ".git",
        ".hg",
        ".venv",
        "venv",
        ".tox",
        ".nox",
        "__pycache__",
        ".forktest",
        ".pytest_cache",
        ".hypothesis",
        ".mypy_cache",
        ".ruff_cache",
        "node_modules",
        "site-packages",
    }
)


IMPORTABLE_BINARY_SUFFIXES = (*importlib.machinery.EXTENSION_SUFFIXES, *importlib.machinery.BYTECODE_SUFFIXES)


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

    Directory listings, edges and closures are memoized for the life of the
    instance, so it describes the tree as it was when first asked. Build a
    new one per run.
    """

    def __init__(self, root: str, fingerprints: Fingerprints, known_modules: Mapping[str, str] | None = None) -> None:
        self.root = root
        self.fps = fingerprints
        self.known = known_modules or {}
        self._files: dict[str, frozenset[str]] = {}
        self._direct: dict[str, frozenset[str]] = {}
        self._closure: dict[str, frozenset[str]] = {}

    def _exists(self, relpath: str) -> bool:
        d, name = posixpath.split(relpath)
        files = self._files.get(d)
        if files is None:
            files = frozenset()
            if not is_excluded(d):
                try:
                    with os.scandir(os.path.join(self.root, d)) as it:
                        files = frozenset(e.name for e in it if e.is_file())
                except OSError:
                    pass
            self._files[d] = files
        return name in files

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
        cached = self._direct.get(path)
        if cached is not None:
            return cached
        edges: set[str] = set()
        if path.endswith(".py"):
            for stmt in self.fps.imports_of(path):
                edges |= self.resolve(stmt, path)
        edges.discard(path)
        result = self._direct[path] = frozenset(edges)
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

    def closure(self, path: str) -> frozenset[str]:
        """Every project file the test file's execution can touch at import time."""
        cached = self._closure.get(path)
        if cached is not None:
            return cached
        seen: set[str] = set()
        stack = [path, *self.conftests_for(path)]
        while stack:
            p = stack.pop()
            if p in seen or not self._exists(p):
                continue
            seen.add(p)
            stack.extend(self.direct(p))
        result = self._closure[path] = frozenset(seen)
        return result


def dep_changed(dep: Dep, current: FingerprintFn) -> bool:
    fp = current(dep.path)
    if fp is None:
        return True
    qualname = dep.qualname if dep.qualname in fp else normalize_qualname(dep.qualname, fp)
    return fp.get(qualname) != dep.hash


def classify(dep: Dep, fixtures: frozenset[tuple[str, str]]) -> Reason:
    name = posixpath.basename(dep.path)
    if dep.qualname == FILE:
        if is_config_file(name):
            return Reason.CONFIG_CHANGE
        return Reason.IMPORT_GRAPH if name.endswith(IMPORTABLE_BINARY_SUFFIXES) else Reason.FILE_READ
    if dep.qualname == MODULE:
        return Reason.CONFIG_CHANGE if name == "conftest.py" else Reason.IMPORT_GRAPH
    return Reason.FIXTURE if (dep.path, dep.qualname) in fixtures else Reason.COVERAGE


class Affected(NamedTuple):
    reasons: dict[str, Reason]
    changed: list[str]


RANK = {reason: rank for rank, reason in enumerate(PRIORITY)}


def select(
    nodeids: Iterable[str],
    view: StoreView,
    current: FingerprintFn,
    graph: ImportGraph,
    *,
    fixtures: frozenset[tuple[str, str]] = frozenset(),
) -> Affected:
    """Each affected nodeid with its highest-priority reason, and the changed paths that affect them.

    Every distinct key is checked once, and only the changed keys' rows of the
    index are read. The import-graph rule depends only on a test's file and
    its recorded module set, so it runs once per distinct pair.
    """
    wanted = list(nodeids)
    changed_keys = {i: d for i, d in view.keys.items() if dep_changed(d, current)}
    by_id = {row.id: nodeid for nodeid in wanted if (row := view.tests.get(nodeid)) is not None}
    reasons: dict[str, Reason] = {}

    def offer(nodeid: str, reason: Reason) -> None:
        held = reasons.get(nodeid)
        if held is None or RANK[reason] < RANK[held]:
            reasons[nodeid] = reason

    paths: set[str] = set()
    for key_id, test_ids in view.tests_using(changed_keys).items():
        dep = changed_keys[key_id]
        reason = classify(dep, fixtures)
        for test_id in test_ids:
            nodeid = by_id.get(test_id)
            if nodeid is not None:
                offer(nodeid, reason)
                paths.add(dep.path)
    groups: dict[tuple[str, int], list[str]] = {}
    for nodeid in wanted:
        row = view.tests.get(nodeid)
        if row is None:
            offer(nodeid, Reason.NEW)
            continue
        if row.outcome in ("failed", "error"):
            offer(nodeid, Reason.LAST_FAILED)
        groups.setdefault((nodeid.split("::")[0], row.modules), []).append(nodeid)
    for (test_file, modules), members in groups.items():
        if not graph.closure(test_file) <= view.module_sets[modules]:
            for nodeid in members:
                offer(nodeid, Reason.IMPORT_GRAPH)
    return Affected(reasons, sorted(paths))


def explain(
    nodeid: str,
    view: StoreView,
    current: FingerprintFn,
    graph: ImportGraph,
    fixtures: frozenset[tuple[str, str]] = frozenset(),
) -> list[tuple[Reason, Dep | None]]:
    """Every reason ``select`` would have for one test, read from that test's own key list."""
    row = view.tests.get(nodeid)
    if row is None:
        return [(Reason.NEW, None)]
    out: list[tuple[Reason, Dep | None]] = []
    if row.outcome in ("failed", "error"):
        out.append((Reason.LAST_FAILED, None))
    deps = [view.keys[k] for k in view.key_ids_of(nodeid)]
    out.extend((classify(d, fixtures), d) for d in deps if dep_changed(d, current))
    unrecorded = graph.closure(nodeid.split("::")[0]) - view.module_sets[row.modules]
    out.extend((Reason.IMPORT_GRAPH, Dep(p, MODULE, "")) for p in sorted(unrecorded))
    return out
