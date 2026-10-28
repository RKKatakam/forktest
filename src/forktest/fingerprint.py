"""Function-level fingerprints of Python source files.

A fingerprint maps every function's qualname (as CPython's ``co_qualname``
spells it) to a hash of its definition with docstrings removed, plus a
``<module>`` entry covering everything that is not a function body.
"""

from __future__ import annotations

import ast
import hashlib
import os
from collections.abc import Iterator
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


def fingerprint(source: bytes | str) -> dict[str, str]:
    tree = ast.parse(source)
    _strip_docstrings(tree)
    out = {qualname: _hash_node(node) for qualname, node in _defs(tree, "")}
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            node.body = [ast.Expr(ast.Constant(...))]
    out[MODULE] = _hash_node(tree)
    return out


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
    out: list[ImportStmt] = []
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Import):
            out.extend(ImportStmt(alias.name, 0, ()) for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            names = tuple(alias.name for alias in node.names if alias.name != "*")
            out.append(ImportStmt(node.module or "", node.level, names))
    return out


class Fingerprints:
    """Current fingerprints of files under ``root``, cached by content hash.

    ``of(path)`` returns ``None`` for a missing file, the function map for a
    ``.py`` file, and ``{"<file>": sha}`` for anything else. A ``.py`` file
    that does not parse gets a single ``<module>`` entry derived from its
    content, so every dependent function counts as changed.
    """

    def __init__(self, root: str) -> None:
        self.root = root
        self._by_sha: dict[str, dict[str, str]] = {}
        self._imports_by_sha: dict[str, list[ImportStmt]] = {}
        self._by_path: dict[str, tuple[int, int, str]] = {}

    def sha(self, path: str) -> str | None:
        full = os.path.join(self.root, path)
        try:
            st = os.stat(full)
        except OSError:
            self._by_path.pop(path, None)
            return None
        cached = self._by_path.get(path)
        if cached and cached[0] == st.st_mtime_ns and cached[1] == st.st_size:
            return cached[2]
        try:
            with open(full, "rb") as f:
                data = f.read()
        except OSError:
            return None
        sha = content_hash(data)
        self._by_path[path] = (st.st_mtime_ns, st.st_size, sha)
        if sha not in self._by_sha:
            self._by_sha[sha] = self._compute(path, data, sha)
        return sha

    def of(self, path: str) -> dict[str, str] | None:
        sha = self.sha(path)
        return None if sha is None else self._by_sha[sha]

    def imports_of(self, path: str) -> list[ImportStmt]:
        sha = self.sha(path)
        if sha is None or not path.endswith(".py"):
            return []
        if sha not in self._imports_by_sha:
            try:
                self._imports_by_sha[sha] = imports(self.read(path))
            except (SyntaxError, ValueError):
                self._imports_by_sha[sha] = []
        return self._imports_by_sha[sha]

    def read(self, path: str) -> bytes:
        with open(os.path.join(self.root, path), "rb") as f:
            return f.read()

    @staticmethod
    def _compute(path: str, data: bytes, sha: str) -> dict[str, str]:
        if not path.endswith(".py"):
            return {FILE: sha}
        try:
            return fingerprint(data)
        except (SyntaxError, ValueError):
            return {MODULE: "unparseable:" + sha}
