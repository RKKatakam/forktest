"""Warm zygotes behind a unix socket, and the watch loop. Filled in next."""

from __future__ import annotations

from collections.abc import Callable

from forktest.events import Event
from forktest.zygote import Request


def run_via_daemon(root: str, request: Request, emit: Callable[[Event], None]) -> bool:
    return False


def watch_command(argv: list[str]) -> int:
    raise SystemExit("forktest watch: not implemented yet")


def daemon_command(argv: list[str]) -> int:
    raise SystemExit("forktest daemon: not implemented yet")
