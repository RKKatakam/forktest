"""Function-level fingerprints of Python source files.

A fingerprint maps every function's qualname (as CPython's ``co_qualname``
spells it) to a hash of its definition with docstrings removed, plus a
``<module>`` entry covering everything that is not a function body.
"""

from __future__ import annotations

import ast
import hashlib
import os
import time
from collections.abc import Iterator, Mapping
from typing import NamedTuple

MODULE = "<module>"
FILE = "<file>"

_SYNTHETIC = ("<lambda>", "<genexpr>", "<listcomp>", "<dictcomp>", "<setcomp>")


def content_hash(data: bytes) -> str:
    return hashlib.blake2b(data, digest_size=10).hexdigest()


def _hash_node(node: ast.AST) -> str:
    return content_hash(ast.dump(node).encode())


def _is_docstring(stmt: ast.stmt) -> bool:
    return (
        isinstance(stmt, ast.Expr)
        and isinstance(stmt.value, ast.Constant)
        and isinstance(stmt.value.value, str)
    )


def _strip_docstrings(tree: ast.AST) -> None:
    for node in ast.walk(tree):
        if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.body and _is_docstring(node.body[0]):
                node.body = node.body[1:]


def _defs(node: ast.AST, prefix: str) -> Iterator[tuple[str, ast.AST]]:
    for child in ast.iter_child_nodes(node):
        if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
            qualname = prefix + child.name
            yield qualname, child
            yield from _defs(child, qualname + ".<locals>.")
        elif isinstance(child, ast.ClassDef):
            yield from _defs(child, prefix + child.name + ".")
        elif isinstance(child, (ast.stmt, ast.ExceptHandler, ast.match_case)):
            yield from _defs(child, prefix)




def normalize_qualname(qualname: str, fp: dict[str, str]) -> str:
    """Map a runtime ``co_qualname`` onto a fingerprint key.

    Lambdas and comprehensions are hashed as part of their enclosing def, and
    anything else without a def of its own (class bodies, module-level
    lambdas) is part of the module skeleton.
    """
    while qualname not in fp:
        head, sep, tail = qualname.rpartition(".<locals>.")
        if not sep or tail not in _SYNTHETIC:
            return MODULE
        qualname = head
    return qualname


class ImportStmt(NamedTuple):
    module: str
    level: int
    names: tuple[str, ...]


def imports(source: bytes | str) -> list[ImportStmt]:
    return _imports(ast.parse(source))


def _imports(tree: ast.AST) -> list[ImportStmt]:
    out: list[ImportStmt] = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            out.extend(ImportStmt(alias.name, 0, ()) for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            names = tuple(alias.name for alias in node.names if alias.name != "*")
            out.append(ImportStmt(node.module or "", node.level, names))
    return out


def analyze(source: bytes | str) -> tuple[dict[str, str], list[ImportStmt]]:
    """The fingerprint and the import statements of one source, from a single parse."""
    tree = ast.parse(source)
    found = _imports(tree)
    _strip_docstrings(tree)
    out = {qualname: _hash_node(node) for qualname, node in _defs(tree, "")}
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            node.body = [ast.Expr(ast.Constant(...))]
    out[MODULE] = _hash_node(tree)
    return out, found


def fingerprint(source: bytes | str) -> dict[str, str]:
    return analyze(source)[0]


class Stat(NamedTuple):
    """What a file's content is trusted by. ``os.utime`` can restore mtime, but any write moves ctime."""

    mtime_ns: int
    ctime_ns: int
    size: int


UNTRUSTED = Stat(0, 0, -1)
RACY_NS = 2_000_000_000


class FileInfo(NamedTuple):
    sha: str
    fingerprint: dict[str, str]
    imports: list[ImportStmt]
    stat: Stat = UNTRUSTED


class Fingerprints:
    """Current fingerprints of files under ``root``.

    ``of(path)`` returns ``None`` for a missing file, the function map for a
    ``.py`` file, and ``{"<file>": sha}`` for anything else. A ``.py`` file
    that does not parse gets a single ``<module>`` entry derived from its
    content, so every dependent function counts as changed.

    ``known`` holds infos from an earlier process, keyed by path. One is
    reused without reading the file while its stat matches, and after a
    read while its content sha matches. Every info that changed here is
    collected in ``fresh`` so the caller can persist it.

    A file read within ``RACY_NS`` of its last change keeps the ``UNTRUSTED``
    stat, as git does for racily clean entries: on a file system with coarse
    timestamps, a second write in the same tick would leave the stat as it was.
    """

    def __init__(self, root: str, known: Mapping[str, FileInfo] | None = None) -> None:
        self.root = root
        self.known = dict(known or {})
        self.fresh: dict[str, FileInfo] = {}

    def info(self, path: str) -> FileInfo | None:
        full = os.path.join(self.root, path)
        try:
            st = os.stat(full)
        except OSError:
            return None
        stat = Stat(st.st_mtime_ns, st.st_ctime_ns, st.st_size)
        known = self.known.get(path)
        if known is not None and known.stat == stat:
            return known
        try:
            with open(full, "rb") as f:
                data = f.read()
        except OSError:
            return None
        sha = content_hash(data)
        info = known if known is not None and known.sha == sha else _compute(path, data, sha)
        if time.time_ns() - max(stat.mtime_ns, stat.ctime_ns) < RACY_NS:
            stat = UNTRUSTED
        info = info._replace(stat=stat)
        if info != known:
            self.known[path] = self.fresh[path] = info
        return info

    def sha(self, path: str) -> str | None:
        info = self.info(path)
        return None if info is None else info.sha

    def of(self, path: str) -> dict[str, str] | None:
        info = self.info(path)
        return None if info is None else info.fingerprint

    def imports_of(self, path: str) -> list[ImportStmt]:
        info = self.info(path)
        return [] if info is None else info.imports


def _compute(path: str, data: bytes, sha: str) -> FileInfo:
    if not path.endswith(".py"):
        return FileInfo(sha, {FILE: sha}, [])
    try:
        return FileInfo(sha, *analyze(data))
    except (SyntaxError, ValueError):
        return FileInfo(sha, {MODULE: "unparseable:" + sha}, [])
