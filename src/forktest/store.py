"""SQLite store of per-test dependency baselines, outcomes and durations."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import time
from collections import defaultdict
from collections.abc import Callable, Iterable, Mapping
from dataclasses import dataclass
from typing import NamedTuple

from forktest.fingerprint import FILE, FileInfo, ImportStmt, content_hash, fingerprint


class Dep(NamedTuple):
    path: str
    qualname: str
    hash: str


class Record(NamedTuple):
    nodeid: str
    outcome: str
    duration: float
    deps: list[Dep]


@dataclass(frozen=True)
class TestRow:
    __test__ = False
    outcome: str
    duration: float
    last_run: float


@dataclass
class StoreView:
    tests: dict[str, TestRow]
    deps: dict[str, list[Dep]]

    def rebased(self, hash_of: Callable[[str, str], str | None]) -> StoreView:
        """The same dependency sets with hashes taken from another baseline.

        ``hash_of(path, qualname)`` returning ``None`` means the function or
        file did not exist there, which counts as changed.
        """
        deps = {
            nodeid: [Dep(d.path, d.qualname, hash_of(d.path, d.qualname) or "") for d in ds]
            for nodeid, ds in self.deps.items()
        }
        return StoreView(self.tests, deps)


SCHEMA = """
CREATE TABLE IF NOT EXISTS tests(
    nodeid TEXT PRIMARY KEY, outcome TEXT NOT NULL, duration REAL NOT NULL, last_run REAL NOT NULL);
CREATE TABLE IF NOT EXISTS deps(
    nodeid TEXT NOT NULL, path TEXT NOT NULL, qualname TEXT NOT NULL, hash TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS deps_nodeid ON deps(nodeid);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files(
    path TEXT PRIMARY KEY, sha TEXT NOT NULL, fingerprint TEXT NOT NULL, imports TEXT NOT NULL);
"""


class Store:
    def __init__(self, path: str) -> None:
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        self.conn = sqlite3.connect(path, timeout=5)
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA synchronous=NORMAL")
        self.conn.executescript(SCHEMA)

    def view(self) -> StoreView:
        tests = {
            nodeid: TestRow(outcome, duration, last_run)
            for nodeid, outcome, duration, last_run in self.conn.execute(
                "SELECT nodeid, outcome, duration, last_run FROM tests"
            )
        }
        deps: dict[str, list[Dep]] = defaultdict(list)
        for nodeid, path, qualname, h in self.conn.execute("SELECT nodeid, path, qualname, hash FROM deps"):
            deps[nodeid].append(Dep(path, qualname, h))
        return StoreView(tests, dict(deps))

    def record(self, records: Iterable[Record]) -> None:
        now = time.time()
        with self.conn:
            for r in records:
                self.conn.execute("DELETE FROM deps WHERE nodeid = ?", (r.nodeid,))
                self.conn.execute(
                    "INSERT OR REPLACE INTO tests VALUES (?, ?, ?, ?)", (r.nodeid, r.outcome, r.duration, now)
                )
                self.conn.executemany(
                    "INSERT INTO deps VALUES (?, ?, ?, ?)", [(r.nodeid, *d) for d in set(r.deps)]
                )

    def file_infos(self) -> dict[str, FileInfo]:
        return {
            path: FileInfo(sha, json.loads(fp), [ImportStmt(m, level, tuple(names)) for m, level, names in json.loads(imps)])
            for path, sha, fp, imps in self.conn.execute("SELECT path, sha, fingerprint, imports FROM files")
        }

    def save_file_infos(self, infos: Mapping[str, FileInfo]) -> None:
        if not infos:
            return
        with self.conn:
            self.conn.executemany(
                "INSERT OR REPLACE INTO files VALUES (?, ?, ?, ?)",
                [(path, i.sha, json.dumps(i.fingerprint), json.dumps(i.imports)) for path, i in infos.items()],
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
