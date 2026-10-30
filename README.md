# forktest

A fast, pytest-compatible test runner. It keeps pytest warm in a zygote
process, forks workers from it, and runs only the tests a change affects.
Your suite, fixtures, markers and `conftest.py` files run on pytest's own
machinery, unchanged.

forktest is one Rust binary. It decides what to run without starting Python,
then drives pytest inside your project's own interpreter. The original
pure-Python build in `src/forktest` is the behavioral reference until the Rust
version replaces it.

## Install

```bash
uv tool install git+https://github.com/RKKatakam/forktest   # needs a Rust toolchain to build
uv tool install .                                           # from a checkout
uvx --from . forktest                                       # run once without installing
```

The wheel holds only the binary, built with maturin as ruff ships. Your tests
run in your project's interpreter, found the way your tooling would:
`FORKTEST_PYTHON` or `--python`, an activated virtualenv (`uv run` and
`poetry run` activate one), a `.venv`, `venv` or `env` directory in the project
or any parent, an activated conda env, Poetry's own env, then `python3` on PATH.

That interpreter needs Python 3.12 or newer and pytest. With an older Python
forktest says so on stderr and runs plain pytest, so the command still works.
Without pytest you get a hint and pytest's own error.

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
{"event":"done","passed":6,"failed":1,"errors":0,"skipped":0,"xfailed":0,"xpassed":0,"duration_ms":52}
```

Passing tests are not emitted as JSON lines; `done` carries their count.
Other events are `collect_error`, `skip`, `xfail`, `xpass`, `error`, `miss`
and `verified`. A strict xpass is reported as `fail`, as pytest does.

Exit codes: 0 all selected tests passed, 1 a test failed or a miss was
found, 2 collection failed, 5 nothing was collected at all.

## How it works

**Fingerprints.** Every `.py` file is parsed with `ast` into a map from
function qualname to a hash of its definition with docstrings removed, plus
a `<module>` hash of the file with every function body replaced by `...`.
Moving or reformatting code changes nothing. Editing a constant, a
signature, an import or a decorator changes the `<module>` hash. Each
file's fingerprint, imports and stat (`mtime_ns`, `ctime_ns`, size) are
kept in the store by path. A later process trusts the stored fingerprint
without reading the file while the stat matches, and after a read while
the content hash matches, so a new zygote or a cold run only reads the
files that changed.

**Recording.** While a test runs, a `sys.monitoring` `PY_START` hook
records which project functions executed, and an `open` audit hook records
which non-Python project files were read. Fixture definitions the test
depends on are added statically, since a session fixture only executes for
the first test in a batch. The static import closure of the test file and
its `conftest.py` files is added as `<module>` dependencies, and so are the
pytest config and lock files. Each worker turns its tests' hits into
dependencies itself, in parallel, and compares a digest of each test's set
with the one stored for it. An unchanged test sends only its result. A
changed one sends its set as positions into dependencies that go over the
pipe once per worker. All of it goes into `.forktest/store.db`, one
baseline per test. Each distinct dependency is stored once, as a key, and
the store keeps an index from each key to the tests that use it, patched
on every write so it never needs a sweep.

**Selection.** A test runs when it is new, failed last time, or any
recorded dependency hash differs from the current one. Each distinct key is
checked once against the current fingerprints, and only the changed keys'
rows of the index are read, so the work grows with the size of the change,
not the suite: 6,825 keys stand for 8.9M recorded references on pandas'
`frame` tests. A test also runs when its file's current import closure
reaches a project file it did not record, such as a newly created module.
That rule depends only on the test file and the set of module paths the
test recorded, so it runs once per distinct pair (124 on pandas). The
import graph's resolved edges are persisted and reused while each file's
imports, the listings of the directories its resolution scanned and the
`sys.modules` map are unchanged. Each selected test carries one reason:
`config_change`, `new`, `last_failed`, `coverage`, `fixture`, `file_read`
or `import_graph`. `--why` prints them.

**Execution.** The collecting process is the zygote. Selected tests are
grouped by module, failed-first, assigned to workers by longest processing
time, using what each test cost its worker last time (the protocol plus
recording, which for a failure or an xfail includes formatting the
traceback). A module that costs more than an even share of the run is
split by class first, then into consecutive runs, so module- and
class-scoped fixtures are rebuilt as rarely as possible. Each worker is an
`os.fork()` of the zygote. It runs its items through the
`pytest_runtest_protocol` hook, as pytest's own loop does, so plugins that
wrap it (the warnings plugin's `filterwarnings` marks, faulthandler,
pytest-timeout) still apply, and it streams results back over a pipe.

**Daemon.** `forktest daemon start` keeps an L0 process with the
third-party modules imported and an L1 forked from it that has collected
the suite. Edits to a file L1 imported, or adding or removing a `.py`
file, kill L1 and re-fork it from L0. Edits to `pyproject.toml` or a
lockfile restart L0. `forktest watch` is the same daemon in the
foreground, driven by file changes, with a niced run of the unselected
tests after each affected run so a selection miss is reported rather than
hidden.

## Measured

All numbers below were measured on a 12-core Apple Silicon laptop with
Python 3.14.6, on 2026-10-28. Other work on the machine kept the load
average between 2 and 6 during these runs, which is why the sides are
interleaved where a table says so. Each number is the median wall-clock
time of one CLI invocation, with the range of the runs after it.

### attrs

A shallow clone of [attrs](https://github.com/python-attrs/attrs) at
c8bb9bf with pytest 9.1.1: 1,413 tests, of which 2 fail in a shallow clone
(`test_packaging.py::TestLegacyMetadataHack::test_version_info`). The edit
adds a line to `_InstanceOfValidator.__repr__` in `src/attr/validators.py`.
forktest selects the 12 tests that run it plus the 2 failing tests. Every
run reported the same pass and fail counts as pytest.

| scenario | runs | median | range |
|---|---|---|---|
| `pytest -q`, whole suite | 5, interleaved | 4.81 s | 4.51 to 4.95 s |
| `pytest -n auto`, whole suite | 5, interleaved | 2.40 s | 2.35 to 2.47 s |
| `forktest` after the edit, no daemon | 5, interleaved | 0.51 s | 0.48 to 0.52 s |
| `forktest --all`, no daemon | 5, interleaved | 1.11 s | 1.08 to 1.15 s |
| `forktest` after the edit, daemon, waits for the zygote to rebuild | 5 | 0.26 s | 0.26 to 0.26 s |
| `forktest` after the edit, daemon, zygote already rebuilt | 5 | 0.098 s | 0.098 to 0.099 s |

Selection takes 13 ms. The code before this work took 49 ms (46 to 50 ms)
for it and 0.52 s per edit run in the same rounds. No `--all` run can beat
its longest test: `tests/test_make.py::TestMetadata::test_not_none_metadata`
takes 0.42 s by pytest's count and 0.59 s with recording.

### pandas

[pandas](https://github.com/pandas-dev/pandas) at c32acd3, built from
source as an editable install (meson and Cython), Python 3.14.6, pytest
9.2.0, without pyarrow. The slice is `pandas/tests/frame`: 16,876 tests, of
which 41 fail in this environment (timezone tests that use dateutil). The
edit adds a line to `describe_ndframe` in `pandas/core/methods/describe.py`.
forktest selects the 55 tests that run it plus the 41 tests that failed
last time. Every run reported the same pass and fail counts as pytest, and
`forktest --all` failed on exactly the same 41 tests as `pytest -n auto`.

| scenario | runs | median | range |
|---|---|---|---|
| `pytest`, whole slice | 5, interleaved | 23.46 s | 23.05 to 24.36 s |
| `pytest -n auto`, whole slice | 5, interleaved | 9.89 s | 9.45 to 11.02 s |
| `forktest` after the edit, no daemon, 96 of 16,876 tests | 5, interleaved | 2.93 s | 2.80 to 3.19 s |
| `forktest --all`, no daemon | 5, interleaved | 7.09 s | 6.34 to 7.48 s |

After an edit, forktest is 8x faster than plain pytest and 3.4x faster
than xdist. Selection takes 86 ms (84 to 92 ms). Most of the rest is
pytest's own import and collection: `pytest --collect-only` alone took
2.21 to 2.39 s in the same minutes. Running the 96 tests takes about 0.4 s,
most of it the 41 failures formatting their tracebacks (3.2 s of worker
time over 12 workers). `--all` is 1.4x faster than xdist. It is bound by
the workers: about 44 s of CPU across 12 of them, of which recording with
`sys.monitoring` is about 12 s and building results and dependency sets
about 4 s. The code before this work took 5.95 s per edit, because
selection loaded and checked all 8.9M recorded references, and 18.76 s for
`--all`, because the parent turned every test's hits into dependencies one
test at a time. Rerun it with `bench/bench_interleaved.py`.

Running pandas found four compatibility bugs, all fixed with a test each:
compiled extensions inside the project crashed recording, `filterwarnings`
marks were skipped (564 false failures), the JUnit reporter imported
`email.message` into the test process (a pandas test that forgets that
import passed under forktest only), and a module skipped at import was not
counted.

### The demo

`uv run python bench/bench.py` prints this table for your machine. Each
side ran 5 times in a row, not interleaved.

| scenario | pytest | forktest |
|---|---|---|
| cold full run | 1.24 s (1.23 to 1.24) | 0.52 s (0.51 to 0.53), `--all` |
| one-function edit, cold process | 1.23 s (1.23 to 1.25), whole suite | 0.23 s (0.23 to 0.24), 7 of 16 selected |
| one-function edit, warm daemon, waits for the zygote to rebuild | | 0.105 s (0.103 to 0.109) |
| one-function edit, warm daemon, zygote already rebuilt | | 0.075 s (0.073 to 0.076) |

The demo's slow module sleeps 0.25 s per test, which is what bounds both
full runs. The cold forktest floor is interpreter start plus `import
pytest` (0.095 s) plus collection.

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
- **Selection still reads one row per test.** Changed keys and the index
  rows they need are small, but every test's outcome, cost and module set
  are loaded to find new and failed tests: about 30 ms of the 86 ms on
  pandas' 16,876 tests.
- **`--all` records coverage on every run.** On pandas, recording is about
  12 s of the 44 s of worker CPU. A test whose dependency set is unchanged
  sends nothing back, but its hits are still collected and hashed.
- **A run that changes every test's dependencies records them serially.**
  The first run, or the first after an upgrade resets the store, makes the
  parent intern and index every test's set: `--all` on pandas took 11 to
  16 s on such runs, against about 7 s afterwards.
- **A file's stat is trusted.** A file whose `mtime_ns`, `ctime_ns` and size
  match the stored ones is not read again. Any write moves ctime, even one
  that restores mtime with `os.utime`, and a file read within 2 s of its
  last change is read again next time, as git does for racily clean files.
  An edit is missed only on a file system that does not update ctime, or
  when the clock moves back.
- **A function behind a cache is recorded by the test that filled it.**
  With `functools.lru_cache`, later tests in the same worker get a cache hit
  and never run the function, so an edit to it does not select them. On
  pandas, about 130 of 16,876 tests record a different set between two
  identical runs, mostly for this reason. `--verify-selection` and the watch safety
  net report such a miss.
- **The daemon does not start on pandas.** L0 preimports
  `_hypothesis_globals`, pytest then warns that it cannot rewrite a module
  already imported, and pandas' `filterwarnings = error` turns that warning
  into a crash of every zygote. L0 also stays alive after it logs that it is
  shutting down. Both happen with the code before this work too.
- pytest-xdist is replaced, not supported.
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
python bench/bench_interleaved.py ROOT TESTPATH EDIT_FILE ANCHOR_LINE ROUNDS LOG   # any real project
```
