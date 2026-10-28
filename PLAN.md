# forktest v0 implementation plan

Scope for this build: the spec's M0 plus the pure-Python core of M2 and M3, all in Python. The Rust core (M1), MCP server, spawn mode, and DB/connection adapters are out of scope. `DESIGN.md` is the product spec.

Target: Python 3.12+ (uses `sys.monitoring`), pytest 8+. Linux and macOS. No runtime dependency beyond pytest.

## User surface

```
forktest [pytest args...]        affected tests only (first run with an empty store runs everything)
forktest --all                   everything, forked and parallel
forktest --since REF             tests affected by function changes between REF and the working tree
forktest --json                  JSONL events on stdout
forktest --junitxml PATH         JUnit XML file
forktest -n N                    worker count (default: os.cpu_count())
forktest --strict-pytest         one worker, one shared session, exact pytest semantics
forktest --verify-selection      after the affected run, run the unselected rest and report any failure among them as a miss
forktest --why NODEID            print why a test would be selected now (reasons), without running
forktest watch                   warm zygotes; rerun affected tests on each save; background safety-net run
forktest daemon start|stop|status  warm zygotes behind a unix socket; plain `forktest` uses it when running
```

Unknown args pass through to pytest (`-k`, `-m`, paths, `-p`, `-x`).

## Data shapes (decided; build around these)

**Fingerprint.** `fingerprint.py` parses a source file with `ast` and returns `dict[qualname, hash]`.
- `qualname` matches CPython's `co_qualname`: `func`, `Cls.method`, `outer.<locals>.inner`.
- Function hash: `ast.dump` of the def with its docstring removed. Line numbers aren't included, so moves, reformatting, and docstring edits don't change it.
- `"<module>"`: a hash of the module with every function body replaced by `...`. It covers module-level statements, class attributes, imports, and signatures.
- Cache by file content sha.

**Dep.** `(path, qualname, hash)`. `path` is relative to rootdir. `qualname` is a function qualname, `"<module>"`, or `"<file>"` for a non-`.py` file read during the test (hash = content sha).

**Store** (`.forktest/store.db`, SQLite).
- `deps(nodeid, path, qualname, hash)`: one baseline per test. Never share a per-file baseline across tests. If a `-k` run skips a test, that test must still see the change on a later run.
- `tests(nodeid PRIMARY KEY, outcome, duration, last_run)`.
- `meta(key, value)`: for example, the third-party module list L0 preimports.
- Replace a test's deps only when that test actually ran.

**Reason** (enum): `new`, `coverage`, `fixture`, `import_graph`, `file_read`, `last_failed`, `config_change`, `all`.

**Selection.** A pure function `select(nodeids, store_view, current_fingerprint_fn, import_graph) -> dict[nodeid, Reason]`, keeping the first matching reason in priority order. It's a pure function so it can be unit tested without pytest.
- `new`: nodeid has no row in `tests`.
- `last_failed`: last outcome was failed or error.
- `coverage` / `fixture` / `file_read`: some dep's recorded hash differs from the current one. A qualname missing from the current fingerprint counts as changed. Before deciding a qualname is missing, strip a `.<locals>.<lambda>` / `<genexpr>` suffix to its enclosing def. A missing file counts as changed.
- `import_graph`: a project module whose `"<module>"` hash changed is transitively imported (static `ast` import graph) by the test's file or by a conftest.py that applies to it. This covers `from config import RATE` with no function call.
- `config_change`: a change in pytest.ini, pyproject.toml, setup.cfg, tox.ini, uv.lock, poetry.lock, requirements*.txt, or any conftest.py `"<module>"` change selects everything (for conftest.py, everything under its directory).
- `--since REF`: the same comparison, but the "recorded" fingerprints come from `git show REF:path` instead of the deps hashes.

**Events** (`events.py`, dataclasses with `to_json()`): `Selection(changed, selected, total, reasons)`, `TestResult(nodeid, outcome, duration, file, line, message, trace)`, `CollectError`, `Miss(nodeid, message)`, `Done(passed, failed, errors, skipped, duration_ms)`. Trace frames keep project files only. Reporters live in a registry `{"human", "jsonl", "junit"}`, and each consumes the event stream. The human and JSONL output match the DESIGN.md examples.

**Worker protocol.** Each worker gets a pipe and writes one JSON line per finished test (the TestResult plus its deps) and a final line. The parent multiplexes the pipes with `selectors`.

**Zygote lifecycle** (daemon and watch). A small explicit state machine: `L1 ∈ {absent, building, ready, stale}`.
- L0 is the long-lived process with the third-party modules from `meta` already imported.
- L1 is forked from L0. It calls `pytest.main(args, plugins=[ZygotePlugin])`.
- The plugin's `pytest_runtestloop` takes over. It applies the selection, schedules, forks workers, streams results, and in daemon mode loops waiting for the next request instead of returning.
- L1 goes `stale` when a `.py` file it imported (from `sys.modules`, under rootdir) changes, or when a `.py` file is added or removed under rootdir. A stale L1 is killed and re-forked from L0. Data-file and non-imported edits keep it `ready`.
- L0 restarts when a lockfile or pyproject changes.

## Mechanisms

- **Execution.** Use `_pytest.runner.runtestprotocol(item, nextitem=..., log=False)` inside the forked worker, chaining `nextitem` within the batch. Suppress pytest's own terminal output in the zygote, but surface collection errors as `CollectError` events.
- **Coverage.** Use `sys.monitoring` `PY_START` with its own tool id.
  - Record the code objects of project files (under rootdir, not in a venv or site-packages) per test.
  - Return `DISABLE` after the first hit and call `restart_events()` between tests.
  - Also add static fixture deps from `item._fixtureinfo.name2fixturedefs`. Session and module fixtures only execute for the first test in a batch, so later tests would miss them otherwise.
  - File reads come from `sys.addaudithook` `open` events on non-`.py` files under rootdir.
- **Scheduling.** Run last-failed first, then by duration. Keep module batches together, assign them to N workers by longest-processing-time, and split a module only when there are fewer modules than workers.
- **Fork safety.** Call `gc.freeze()` before forking. If `threading.active_count() > 1` in the zygote, warn and run in strict single-worker mode. On macOS, re-exec once with `OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES` if it isn't set. Workers call `os._exit`.
- **Safety net.** `--verify-selection`, and in watch mode a background run of the unselected tests after each affected run, executed under `os.nice(10)` and cancelled by the next change. Report a failing unselected test as `Miss`.
- **File watching.** mtime polling of files under rootdir, skipping `.git`, `.venv`, `__pycache__`, and `.forktest`. No extra dependency.

## Layout

```
pyproject.toml            hatchling, [project.scripts] forktest = "forktest.cli:main"
src/forktest/             __init__, __main__, cli, fingerprint, store, selection, events, reporters, zygote, worker, daemon
tests/                    forktest's own suite: unit tests for fingerprint/selection/store, end-to-end tests that run the CLI in a temp copy of examples/demo
examples/demo/            small realistic project (billing/tax.py, billing/invoice.py, config.py with module constants, a data file read by a test, conftest.py with a session fixture, parametrized tests, a class-based test, one deliberately slow module)
bench/bench.py            times pytest vs forktest (cold full, incremental after a one-function edit, warm daemon incremental) and prints a table
README.md                 install, usage, what works, honest limitations
```

## Done means

1. `uv run pytest tests` passes.
2. On `examples/demo`, end to end through the real CLI:
   - the first run selects all
   - an immediate rerun selects 0
   - editing one function body selects exactly the tests that execute it
   - editing a docstring selects 0
   - editing a module constant selects its importers
   - editing the data file selects its reader
   - a failing test reruns next time as `last_failed`
   - `--json` lines parse
   - `--junitxml` is valid XML
   - `--verify-selection` reports 0 misses after each of the edits above
3. Results (pass/fail counts) match plain `pytest` on the demo and on one real cloned repo.
