# forktest

A fast, pytest-compatible test runner. It keeps pytest warm in a zygote
process, forks workers from it, and runs only the tests a change affects.
Your suite, fixtures, markers and `conftest.py` files run on pytest's own
machinery, unchanged.

This is the v0 pure-Python build described in `PLAN.md`. `DESIGN.md` is the
product spec. The Rust daemon, MCP server, spawn mode and database adapters
from the spec are not here yet.

## Install

Python 3.12 or newer and pytest 8 or newer.

```bash
uv tool install .          # from a checkout
# or, inside a project venv
uv pip install -e /path/to/forktest
```

## Usage

```bash
forktest                     # tests affected by your uncommitted edits (first run: everything)
forktest --all               # the whole suite, forked and parallel
forktest --since main        # affected by every function changed since branching from main
forktest --json              # one JSON object per line, for agents and CI
forktest --junitxml out.xml  # JUnit XML as well
forktest -n 4                # worker count (default: cpu count)
forktest --strict-pytest     # one process, one shared session, exact pytest semantics
forktest --verify-selection  # also run the unselected tests; a failure among them is a "miss"
forktest --why tests/test_x.py::test_y   # why that test would run now, without running it
forktest -k billing -x       # anything forktest does not know passes through to pytest

forktest watch               # rerun affected tests on every save, safety net in the background
forktest daemon start        # keep warm zygotes behind a unix socket; plain `forktest` uses them
forktest daemon status
forktest daemon stop
```

Human output:

```
forktest · 1 file changed · 7 of 16 tests selected (0.01s)
  ✗ tests/test_tax.py::test_round_vat[10.005-10.01]
      tests/test_tax.py:14  assert Decimal('11.01') == Decimal('10.01')
      +  where Decimal('11.01') = round_vat(Decimal('10.005'))
  ✓ tests/test_invoice.py   2 passed
1 failed, 6 passed in 0.05s
```

JSON output (`--json`):

```json
{"event":"selection","changed":["billing/tax.py"],"selected":7,"total":16,"reasons":{"coverage":7},"duration_ms":5}
{"event":"fail","test":"tests/test_tax.py::test_round_vat[10.005-10.01]","duration_ms":0.4,"file":"tests/test_tax.py","line":14,"message":"assert Decimal('11.01') == Decimal('10.01')\n +  where ...","trace":[{"file":"tests/test_tax.py","line":14,"fn":"test_round_vat"}]}
{"event":"done","passed":6,"failed":1,"errors":0,"skipped":0,"duration_ms":52}
```

Passing tests are not emitted as JSON lines; `done` carries their count.
Other events are `collect_error`, `skip`, `error`, `miss` and `verified`.

Exit codes: 0 all selected tests passed, 1 a test failed or a miss was
found, 2 collection failed, 5 nothing was collected at all.

## How it works

**Fingerprints.** Every `.py` file is parsed with `ast` into a map from
function qualname to a hash of its definition with docstrings removed, plus
a `<module>` hash of the file with every function body replaced by `...`.
Moving or reformatting code changes nothing. Editing a constant, a
signature, an import or a decorator changes the `<module>` hash.

**Recording.** While a test runs, a `sys.monitoring` `PY_START` hook
records which project functions executed, and an `open` audit hook records
which non-Python project files were read. Fixture definitions the test
depends on are added statically, since a session fixture only executes for
the first test in a batch. The static import closure of the test file and
its `conftest.py` files is added as `<module>` dependencies, and so are the
pytest config and lock files. All of it goes into `.forktest/store.db`, one
baseline per test.

**Selection.** A test runs when it is new, failed last time, or any
recorded dependency hash differs from the current one. Each selected test
carries one reason: `config_change`, `new`, `last_failed`, `coverage`,
`fixture`, `file_read` or `import_graph`. `--why` prints them.

**Execution.** The collecting process is the zygote. Selected tests are
grouped by module, failed-first, assigned to workers by longest processing
time, and each worker is an `os.fork()` of the zygote that runs its items
through pytest's `runtestprotocol` and streams results back over a pipe.

**Daemon.** `forktest daemon start` keeps an L0 process with the
third-party modules imported and an L1 forked from it that has collected
the suite. Edits to a file L1 imported, or adding or removing a `.py`
file, kill L1 and re-fork it from L0. Edits to `pyproject.toml` or a
lockfile restart L0. `forktest watch` is the same daemon in the
foreground, driven by file changes, with a niced run of the unselected
tests after each affected run so a selection miss is reported rather than
hidden.

## Measured on the demo

`uv run python bench/bench.py` prints the table for your machine. On a
12-core Apple Silicon laptop with Python 3.12, median of 5 runs each:

| scenario | pytest | forktest |
|---|---|---|
| cold full run | 1.19 s | 0.44 s (`--all`) |
| one-function edit, cold process | 1.18 s (whole suite) | 0.17 s (7 of 16 selected) |
| one-function edit, warm daemon, zygote already rebuilt | | 0.058 s |
| one-function edit, warm daemon, waits for the zygote to rebuild | | 0.086 s |

The demo's slow module sleeps 0.25 s per test, which is what bounds both
full runs. The cold forktest floor is interpreter start plus `import
pytest` (0.08 s) plus collection.

## What works

- Fixtures of every scope, `conftest.py` hierarchies, parametrize,
  markers, `-k`, `-m`, `-x`, paths and other pytest arguments.
- Assertion rewriting and pytest's failure explanations.
- Function-level selection, data-file selection, module-constant
  selection through the import graph, config-change detection.
- JSONL, JUnit and human reporters; `--verify-selection` and the watch
  safety net.

## Limitations, honestly

- **Each worker builds session fixtures itself.** There is no L2 layer yet,
  so a session fixture runs once per worker rather than once per run, and a
  test that mutates a session fixture does not affect tests in other
  workers. `--strict-pytest` gives exact single-process semantics.
- **Plugins that wrap `pytest_runtest_protocol`** (pytest-rerunfailures,
  pytest-timeout's protocol hook, flaky) are bypassed, because workers call
  `runtestprotocol` directly. Hooks on setup, call, teardown and makereport
  still run. pytest-xdist is replaced, not supported.
- **Captured output is not shown.** Failure events carry the assertion
  message and the project frames, not the captured stdout of the test.
- **Hashes are read after the test ran.** An edit saved while a run is in
  progress can be recorded against the wrong content. The safety net and
  the next full run catch it.
- **Same-second, same-size edits can run stale bytecode.** CPython's
  `.pyc` check uses the source mtime in whole seconds and the size. This
  is a CPython behaviour that plain pytest shares. forktest's selection is
  content-based and will still pick the right tests.
- **Threads in the zygote disable forking.** If anything started a thread
  during collection, forktest warns and runs in-process.
- **macOS.** forktest re-execs itself once with
  `OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES` before forking. Linux is the
  primary target.
- **Windows** is not supported.
- Static collection, the MCP server, spawn mode and the database and
  connection adapters from `DESIGN.md` are not implemented.

## Development

```bash
uv sync
uv run pytest tests          # unit tests plus CLI, daemon and watch end-to-end tests on examples/demo
uv run python bench/bench.py
```
