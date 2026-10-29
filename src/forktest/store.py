"""SQLite store of per-test dependency baselines, outcomes and durations."""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import sqlite3
import subprocess
import time
from array import array
from collections import defaultdict
from collections.abc import Callable, Collection, Iterable, Mapping
from dataclasses import dataclass
from typing import NamedTuple

from forktest.fingerprint import FILE, MODULE, FileInfo, ImportStmt, Stat, content_hash, fingerprint


class Dep(NamedTuple):
    path: str
    qualname: str
    hash: str


class Deps(NamedTuple):
    keys: list[Dep]
    digest: str

    @classmethod
    def of(cls, deps: Iterable[Dep]) -> Deps:
        keys = sorted(set(deps))
        return cls(keys, deps_digest(keys))


def dep_hash(dep: Dep) -> int:
    return int.from_bytes(hashlib.blake2b("\0".join(dep).encode(), digest_size=16).digest())


def deps_digest(deps: Collection[Dep], hash_of: Callable[[Dep], int] = dep_hash) -> str:
    """The sum of the distinct deps' hashes mod 2**128, which needs no sort, so workers can cache each dep's hash."""
    return format(sum(map(hash_of, deps)) & ((1 << 128) - 1), "032x")


class Record(NamedTuple):
    nodeid: str
    outcome: str
    duration: float
    deps: Deps | None
    """``None`` when the test's dependency set has the digest stored for it."""


class TestRow(NamedTuple):
    id: int
    outcome: str
    duration: float
    last_run: float
    modules: int
    digest: str


def _ids(blob: bytes) -> array[int]:
    out = array("I")
    out.frombytes(blob)
    return out


def _json_ids(ids: Iterable[int]) -> str:
    return json.dumps(list(ids))


@dataclass
class StoreView:
    """What selection reads up front: every test's row, every distinct
    dependency (``keys``), and the distinct sets of ``<module>`` and
    ``<file>`` paths tests recorded (``module_sets``, which a row's
    ``modules`` points into). The key to tests index and each test's own key
    list are the bulk of the store, so they are read on demand.
    """

    tests: dict[str, TestRow]
    keys: dict[int, Dep]
    module_sets: dict[int, frozenset[str]]
    conn: sqlite3.Connection

    def tests_using(self, key_ids: Iterable[int]) -> dict[int, array[int]]:
        rows = self.conn.execute(
            "SELECT key_id, tests FROM key_tests WHERE key_id IN (SELECT value FROM json_each(?))", (_json_ids(key_ids),)
        )
        return {k: _ids(blob) for k, blob in rows}

    def key_ids_of(self, nodeid: str) -> array[int]:
        row = self.conn.execute(
            "SELECT deps FROM test_deps JOIN tests ON tests.id = test_id WHERE nodeid = ?", (nodeid,)
        ).fetchone()
        return _ids(row[0]) if row else array("I")

    def rebased(self, hash_of: Callable[[str, str], str | None]) -> StoreView:
        """The same dependency sets with hashes taken from another baseline.

        ``hash_of(path, qualname)`` returning ``None`` means the function or
        file did not exist there, which counts as changed.
        """
        keys = {i: Dep(d.path, d.qualname, hash_of(d.path, d.qualname) or "") for i, d in self.keys.items()}
        return dataclasses.replace(self, keys=keys)


SCHEMA_VERSION = 6
SCHEMA = """
CREATE TABLE IF NOT EXISTS tests(
    id INTEGER PRIMARY KEY, nodeid TEXT NOT NULL UNIQUE, outcome TEXT NOT NULL, duration REAL NOT NULL,
    last_run REAL NOT NULL, modules INTEGER NOT NULL, digest TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS test_deps(test_id INTEGER PRIMARY KEY, deps BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS dep_keys(
    id INTEGER PRIMARY KEY, path TEXT NOT NULL, qualname TEXT NOT NULL, hash TEXT NOT NULL,
    UNIQUE(path, qualname, hash));
CREATE TABLE IF NOT EXISTS key_tests(key_id INTEGER PRIMARY KEY, tests BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS module_sets(id INTEGER PRIMARY KEY, paths TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
    path TEXT PRIMARY KEY, sha TEXT NOT NULL, mtime_ns INTEGER NOT NULL, ctime_ns INTEGER NOT NULL,
    size INTEGER NOT NULL, fingerprint TEXT NOT NULL, imports TEXT NOT NULL);
"""
TABLES = ("tests", "deps", "test_deps", "dep_keys", "key_tests", "module_sets", "files")


class _IndexDelta:
    """Per key, the test ids to add to and remove from ``key_tests``."""

    def __init__(self) -> None:
        self.added: defaultdict[int, list[int]] = defaultdict(list)
        self.removed: defaultdict[int, set[int]] = defaultdict(set)

    def replace(self, test_id: int, previous: array[int], current: array[int]) -> None:
        if previous == current:
            return
        before, after = set(previous), set(current)
        for k in after - before:
            self.added[k].append(test_id)
        for k in before - after:
            self.removed[k].add(test_id)


class Store:
    """Each distinct ``Dep`` is a ``dep_keys`` row, and ``test_deps`` holds a
    packed uint32 array of key ids per test, apart from ``tests`` so that
    loading every test's row stays a narrow scan. ``key_tests`` is the
    inverse, a packed array of test ids per key, kept exact on every write:
    a key goes away with the last test that uses it, so every key in the
    store is live.
    """

    def __init__(self, path: str) -> None:
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        self.conn = sqlite3.connect(path, timeout=5)
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA synchronous=NORMAL")
        with self.conn:
            if self.conn.execute("PRAGMA user_version").fetchone()[0] != SCHEMA_VERSION:
                for table in TABLES:
                    self.conn.execute(f"DROP TABLE IF EXISTS {table}")
                self.conn.execute(f"PRAGMA user_version = {SCHEMA_VERSION}")
            self.conn.executescript(SCHEMA)

    def view(self) -> StoreView:
        keys = {i: Dep(p, q, h) for i, p, q, h in self.conn.execute("SELECT id, path, qualname, hash FROM dep_keys")}
        sets = {i: frozenset(json.loads(paths)) for i, paths in self.conn.execute("SELECT id, paths FROM module_sets")}
        tests = {
            nodeid: TestRow(i, outcome, duration, last_run, modules, digest)
            for i, nodeid, outcome, duration, last_run, modules, digest in self.conn.execute(
                "SELECT id, nodeid, outcome, duration, last_run, modules, digest FROM tests"
            )
        }
        return StoreView(tests, keys, sets, self.conn)

    def record(self, records: Iterable[Record]) -> None:
        latest = {r.nodeid: r for r in records}
        if not latest:
            return
        now = time.time()
        with self.conn:
            # The index is read, patched and written back, so take the write lock before the first read.
            self.conn.execute("BEGIN IMMEDIATE")
            self.conn.executemany(
                "UPDATE tests SET outcome = ?, duration = ?, last_run = ? WHERE nodeid = ?",
                [(r.outcome, r.duration, now, r.nodeid) for r in latest.values() if r.deps is None],
            )
            changed = [r for r in latest.values() if r.deps is not None]
            if changed:
                self._record_deps(changed, now)

    def _record_deps(self, records: list[Record], now: float) -> None:
        key_ids = {Dep(p, q, h): i for i, p, q, h in self.conn.execute("SELECT id, path, qualname, hash FROM dep_keys")}
        set_ids = {paths: i for i, paths in self.conn.execute("SELECT id, paths FROM module_sets")}
        old = self._rows(r.nodeid for r in records)
        delta = _IndexDelta()
        for r in records:
            ids = []
            for d in r.deps.keys:
                i = key_ids.get(d)
                if i is None:
                    i = key_ids[d] = self.conn.execute(
                        "INSERT INTO dep_keys(path, qualname, hash) VALUES (?, ?, ?)", d
                    ).lastrowid
                ids.append(i)
            packed = array("I", sorted(ids))
            paths = json.dumps(sorted({d.path for d in r.deps.keys if d.qualname in (MODULE, FILE)}))
            modules = set_ids.get(paths)
            if modules is None:
                modules = set_ids[paths] = self.conn.execute("INSERT INTO module_sets(paths) VALUES (?)", (paths,)).lastrowid
            row = (r.outcome, r.duration, now, modules, r.deps.digest)
            previous = None
            if r.nodeid in old:
                test_id, previous = old[r.nodeid]
                self.conn.execute(
                    "UPDATE tests SET outcome = ?, duration = ?, last_run = ?, modules = ?, digest = ? WHERE id = ?",
                    (*row, test_id),
                )
            else:
                test_id = self.conn.execute(
                    "INSERT INTO tests(outcome, duration, last_run, modules, digest, nodeid) VALUES (?, ?, ?, ?, ?, ?)",
                    (*row, r.nodeid),
                ).lastrowid
            if previous != packed:
                self.conn.execute("INSERT OR REPLACE INTO test_deps VALUES (?, ?)", (test_id, packed.tobytes()))
            delta.replace(test_id, previous or array("I"), packed)
        self._apply(delta)

    def forget(self, nodeids: Collection[str]) -> None:
        """Drop tests that no longer exist, and every key only they used."""
        if not nodeids:
            return
        with self.conn:
            self.conn.execute("BEGIN IMMEDIATE")
            delta = _IndexDelta()
            rows = self._rows(nodeids).values()
            for test_id, previous in rows:
                delta.replace(test_id, previous, array("I"))
            ids = [(test_id,) for test_id, _ in rows]
            self.conn.executemany("DELETE FROM tests WHERE id = ?", ids)
            self.conn.executemany("DELETE FROM test_deps WHERE test_id = ?", ids)
            self._apply(delta)

    def _rows(self, nodeids: Iterable[str]) -> dict[str, tuple[int, array[int]]]:
        rows = self.conn.execute(
            "SELECT nodeid, id, deps FROM tests JOIN test_deps ON test_id = id"
            " WHERE nodeid IN (SELECT value FROM json_each(?))",
            (json.dumps(list(nodeids)),),
        )
        return {nodeid: (i, _ids(blob)) for nodeid, i, blob in rows}

    def _apply(self, delta: _IndexDelta) -> None:
        touched = delta.added.keys() | delta.removed.keys()
        current = {
            k: _ids(blob)
            for k, blob in self.conn.execute(
                "SELECT key_id, tests FROM key_tests WHERE key_id IN (SELECT value FROM json_each(?))", (_json_ids(touched),)
            )
        }
        keep, drop = [], []
        for k in touched:
            tests = current.get(k, array("I"))
            removed = delta.removed.get(k)
            if removed:
                tests = array("I", (t for t in tests if t not in removed))
            tests.extend(delta.added.get(k, ()))
            if tests:
                keep.append((k, tests.tobytes()))
            else:
                drop.append((k,))
        self.conn.executemany("INSERT OR REPLACE INTO key_tests VALUES (?, ?)", keep)
        self.conn.executemany("DELETE FROM key_tests WHERE key_id = ?", drop)
        self.conn.executemany("DELETE FROM dep_keys WHERE id = ?", drop)
        self.conn.execute("DELETE FROM module_sets WHERE id NOT IN (SELECT modules FROM tests)")

    def file_infos(self) -> dict[str, FileInfo]:
        return {
            path: FileInfo(
                sha,
                json.loads(fp),
                [ImportStmt(m, level, tuple(names)) for m, level, names in json.loads(imps)],
                Stat(mtime_ns, ctime_ns, size),
            )
            for path, sha, mtime_ns, ctime_ns, size, fp, imps in self.conn.execute(
                "SELECT path, sha, mtime_ns, ctime_ns, size, fingerprint, imports FROM files"
            )
        }

    def save_file_infos(self, infos: Mapping[str, FileInfo]) -> None:
        if not infos:
            return
        with self.conn:
            self.conn.executemany(
                "INSERT OR REPLACE INTO files VALUES (?, ?, ?, ?, ?, ?, ?)",
                [(path, i.sha, *i.stat, json.dumps(i.fingerprint), json.dumps(i.imports)) for path, i in infos.items()],
            )

    def meta_get(self, key: str) -> str | None:
        row = self.conn.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
        return row[0] if row else None

    def meta_set(self, key: str, value: str) -> None:
        with self.conn:
            self.conn.execute("INSERT OR REPLACE INTO meta VALUES (?, ?)", (key, value))

    def close(self) -> None:
        self.conn.close()


class GitBaseline:
    """Fingerprints of files as they were at a git ref, for ``--since``.

    The ref is resolved to its merge base with HEAD, so ``--since main`` means
    "since this branch left main" even after main moved on.
    """

    def __init__(self, root: str, ref: str) -> None:
        merge_base = subprocess.run(
            ["git", "merge-base", "HEAD", ref], cwd=root, capture_output=True, text=True
        )
        self.sha = merge_base.stdout.strip() if merge_base.returncode == 0 else ref
        self.proc = subprocess.Popen(
            ["git", "cat-file", "--batch"], cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE
        )
        self._cache: dict[str, dict[str, str] | None] = {}

    def read(self, path: str) -> bytes | None:
        assert self.proc.stdin and self.proc.stdout
        self.proc.stdin.write(f"{self.sha}:{path}\n".encode())
        self.proc.stdin.flush()
        header = self.proc.stdout.readline().decode().split()
        if len(header) < 3:
            return None
        size = int(header[2])
        data = self.proc.stdout.read(size)
        self.proc.stdout.read(1)
        return data

    def fingerprint(self, path: str) -> dict[str, str] | None:
        if path not in self._cache:
            data = self.read(path)
            if data is None:
                fp = None
            elif path.endswith(".py"):
                try:
                    fp = fingerprint(data)
                except (SyntaxError, ValueError):
                    fp = {}
            else:
                fp = {FILE: content_hash(data)}
            self._cache[path] = fp
        return self._cache[path]

    def hash_of(self, path: str, qualname: str) -> str | None:
        fp = self.fingerprint(path)
        return None if fp is None else fp.get(qualname)

    def close(self) -> None:
        if self.proc.stdin:
            self.proc.stdin.close()
        self.proc.wait()
