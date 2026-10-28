"""SQLite store of per-test dependency baselines, outcomes and durations."""

from __future__ import annotations

import os
import sqlite3
import time
from collections import defaultdict
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from typing import NamedTuple


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

    def meta_get(self, key: str) -> str | None:
        row = self.conn.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
        return row[0] if row else None

    def meta_set(self, key: str, value: str) -> None:
        with self.conn:
            self.conn.execute("INSERT OR REPLACE INTO meta VALUES (?, ?)", (key, value))

    def close(self) -> None:
        self.conn.close()
