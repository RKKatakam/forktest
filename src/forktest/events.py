"""What a client asks for (Request) and the event stream a run produces.

Reporters consume events; the daemon ships both over its socket.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from typing import Any, TypedDict


class Frame(TypedDict):
    file: str
    line: int
    fn: str


@dataclass
class Selection:
    changed: list[str]
    selected: int
    total: int
    reasons: dict[str, int]
    duration_ms: int = 0

    def to_json(self) -> dict[str, Any]:
        return {"event": "selection", **asdict(self)}


@dataclass
class TestResult:
    nodeid: str
    outcome: str
    duration: float
    file: str | None = None
    line: int | None = None
    message: str | None = None
    trace: list[Frame] = field(default_factory=list)

    EVENT_NAMES = {"passed": "pass", "failed": "fail", "skipped": "skip", "error": "error"}
    __test__ = False

    def to_json(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "event": self.EVENT_NAMES[self.outcome],
            "test": self.nodeid,
            "duration_ms": round(self.duration * 1000, 1),
        }
        if self.message is not None:
            out.update(file=self.file, line=self.line, message=self.message, trace=self.trace)
        return out


@dataclass
class CollectError:
    nodeid: str
    message: str

    def to_json(self) -> dict[str, Any]:
        return {"event": "collect_error", **asdict(self)}


@dataclass
class Miss:
    nodeid: str
    message: str

    def to_json(self) -> dict[str, Any]:
        return {"event": "miss", "test": self.nodeid, "message": self.message}


@dataclass
class Verified:
    checked: int
    misses: int
    duration_ms: int

    def to_json(self) -> dict[str, Any]:
        return {"event": "verified", **asdict(self)}


@dataclass
class Done:
    passed: int
    failed: int
    errors: int
    skipped: int
    duration_ms: int

    def to_json(self) -> dict[str, Any]:
        return {"event": "done", **asdict(self)}


Event = Selection | TestResult | CollectError | Miss | Verified | Done

_OUTCOMES = {name: outcome for outcome, name in TestResult.EVENT_NAMES.items()}


def dumps(event: Event) -> str:
    return json.dumps(event.to_json(), separators=(",", ":"))


def parse(line: str | bytes) -> Event:
    d = json.loads(line)
    kind = d.pop("event")
    if kind in _OUTCOMES:
        return TestResult(
            nodeid=d["test"],
            outcome=_OUTCOMES[kind],
            duration=d["duration_ms"] / 1000,
            file=d.get("file"),
            line=d.get("line"),
            message=d.get("message"),
            trace=d.get("trace", []),
        )
    if kind == "miss":
        return Miss(d["test"], d["message"])
    cls = {
        "selection": Selection,
        "collect_error": CollectError,
        "verified": Verified,
        "done": Done,
    }[kind]
    return cls(**d)


@dataclass
class Request:
    mode: str = "affected"
    since: str | None = None
    verify: bool = False
    workers: int = 0
    strict: bool = False
    nice: bool = False
    pytest_args: list[str] = field(default_factory=list)

    def to_json(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_json(cls, d: dict[str, Any]) -> Request:
        return cls(**d)
