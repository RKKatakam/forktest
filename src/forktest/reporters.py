"""Reporters turn the event stream into human, JSONL, or JUnit output."""

from __future__ import annotations

import sys
from collections import defaultdict
from typing import IO, Protocol
from xml.sax.saxutils import escape, quoteattr

from forktest.events import (
    CollectError,
    Done,
    Event,
    Miss,
    Selection,
    TestResult,
    Verified,
    dumps,
)


class Reporter(Protocol):
    def handle(self, event: Event) -> None: ...
    def close(self) -> None: ...


class JsonlReporter:
    """One JSON object per line. Passing tests are summarized in ``done``."""

    def __init__(self, out: IO[str]) -> None:
        self.out = out

    def handle(self, event: Event) -> None:
        if isinstance(event, TestResult) and event.outcome == "passed":
            return
        self.out.write(dumps(event) + "\n")
        self.out.flush()

    def close(self) -> None:
        pass


class HumanReporter:
    """Compact terminal output: failures stream as they arrive, files summarize at the end."""

    def __init__(self, out: IO[str], expect_verify: bool = False) -> None:
        self.out = out
        self.expect_verify = expect_verify
        self.color = hasattr(out, "isatty") and out.isatty()
        self.per_file: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))

    def _mark(self, ok: bool) -> str:
        glyph = "✓" if ok else "✗"
        if not self.color:
            return glyph
        return f"\033[32m{glyph}\033[0m" if ok else f"\033[31m{glyph}\033[0m"

    def _emit(self, text: str) -> None:
        self.out.write(text + "\n")
        self.out.flush()

    def handle(self, event: Event) -> None:
        if isinstance(event, Selection):
            self._selection(event)
        elif isinstance(event, TestResult):
            self._result(event)
        elif isinstance(event, CollectError):
            self._emit(f"  {self._mark(False)} collection error in {event.nodeid}")
            self._emit(_indent(event.message, 6, limit=12))
        elif isinstance(event, Miss):
            self._emit(f"  {self._mark(False)} MISS {event.nodeid} (not selected, failed in safety net)")
            self._emit(_indent(event.message, 6))
        elif isinstance(event, Verified):
            tail = "0 misses" if event.misses == 0 else f"{event.misses} MISSED"
            self._emit(f"safety net: {event.checked} unselected tests ran in {event.duration_ms / 1000:.2f}s · {tail}")
        elif isinstance(event, Done):
            self._done(event)

    def _selection(self, s: Selection) -> None:
        if s.reasons.get("all") == s.selected and s.selected:
            head = f"all {s.total:,} tests selected"
        elif s.reasons.get("new") == s.total and s.total:
            head = f"no history · {s.selected:,} of {s.total:,} tests selected"
        else:
            n = len(s.changed)
            head = f"{n} file{'s' if n != 1 else ''} changed · {s.selected:,} of {s.total:,} tests selected"
        self._emit(f"forktest · {head} ({s.duration_ms / 1000:.2f}s)")

    def _result(self, r: TestResult) -> None:
        file = r.nodeid.split("::")[0]
        self.per_file[file][r.outcome] += 1
        if r.outcome in ("failed", "error"):
            self._emit(f"  {self._mark(False)} {r.nodeid}")
            where = f"{r.file}:{r.line}  " if r.file else ""
            lines = (r.message or "").splitlines() or [""]
            self._emit(f"      {where}{lines[0]}")
            for line in lines[1:6]:
                self._emit(f"      {line.strip()}")

    def _done(self, d: Done) -> None:
        if self.per_file:
            width = max(len(f) for f in self.per_file)
            for file, counts in self.per_file.items():
                if counts.get("failed") or counts.get("error"):
                    continue
                parts = [f"{counts[k]} {k}" for k in ("passed", "skipped") if counts.get(k)]
                self._emit(f"  {self._mark(True)} {file.ljust(width)}   {', '.join(parts)}")
        parts = [
            f"{d.failed} failed" if d.failed else "",
            f"{d.errors} error{'s' if d.errors != 1 else ''}" if d.errors else "",
            f"{d.passed} passed",
            f"{d.skipped} skipped" if d.skipped else "",
        ]
        summary = ", ".join(p for p in parts if p)
        if d.passed + d.failed + d.errors + d.skipped == 0:
            summary = "nothing to run"
        tail = " · safety net: running unselected tests" if self.expect_verify else ""
        self._emit(f"{summary} in {d.duration_ms / 1000:.2f}s{tail}")

    def close(self) -> None:
        pass


def _indent(text: str, n: int, limit: int | None = None) -> str:
    lines = text.splitlines() or [""]
    if limit and len(lines) > limit:
        lines = lines[: limit - 1] + [f"... {len(lines) - limit + 1} more lines"]
    return "\n".join(" " * n + line for line in lines)


class JunitReporter:
    def __init__(self, path: str) -> None:
        self.path = path
        self.results: list[TestResult] = []
        self.collect_errors: list[CollectError] = []
        self.done: Done | None = None

    def handle(self, event: Event) -> None:
        if isinstance(event, TestResult):
            self.results.append(event)
        elif isinstance(event, CollectError):
            self.collect_errors.append(event)
        elif isinstance(event, Done):
            self.done = event

    def close(self) -> None:
        d = self.done or Done(0, 0, 0, 0, 0)
        errors = d.errors + len(self.collect_errors)
        lines = [
            '<?xml version="1.0" encoding="utf-8"?>',
            f'<testsuites><testsuite name="forktest" tests="{len(self.results)}" failures="{d.failed}" '
            f'errors="{errors}" skipped="{d.skipped}" time="{d.duration_ms / 1000:.3f}">',
        ]
        for r in self.results:
            classname, _, name = r.nodeid.rpartition("::")
            classname = classname.replace("/", ".").removesuffix(".py").replace("::", ".")
            head = f"<testcase classname={quoteattr(classname)} name={quoteattr(name)} time=\"{r.duration:.3f}\""
            if r.outcome == "passed":
                lines.append(head + " />")
                continue
            tag = {"failed": "failure", "error": "error", "skipped": "skipped"}[r.outcome]
            msg = (r.message or "").splitlines()[0] if r.message else ""
            lines.append(f"{head}><{tag} message={quoteattr(msg)}>{escape(r.message or '')}</{tag}></testcase>")
        for ce in self.collect_errors:
            lines.append(
                f"<testcase classname=\"collection\" name={quoteattr(ce.nodeid)} time=\"0\">"
                f"<error message=\"collection error\">{escape(ce.message)}</error></testcase>"
            )
        lines.append("</testsuite></testsuites>")
        with open(self.path, "w", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")


REPORTERS = {"human": HumanReporter, "jsonl": JsonlReporter, "junit": JunitReporter}


def human(out: IO[str] | None = None, expect_verify: bool = False) -> HumanReporter:
    return HumanReporter(out or sys.stdout, expect_verify)
