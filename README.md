# forktest

A fast, pytest-compatible test runner. It decides which tests a change
affects before Python starts, keeps pytest warm in a zygote, forks workers
from it, and runs only those tests. Your suite, fixtures, markers, plugins and
`conftest.py` files run on pytest's own machinery, unchanged.

forktest is one Rust binary that drives pytest inside your project's own
interpreter. It never forks a process that has threads, so suites that use
JAX, torch or anything else with native thread pools run correctly. The
original pure-Python version stays on the `main` branch as the behavioral
reference.

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
forktest -n 4                # at most 4 workers (default: cpu count; fewer when the selection is cheap)
forktest --strict-pytest     # one process, one shared session, exact pytest semantics
forktest --verify-selection  # also run the unselected tests; a failure among them is a "miss"
forktest --why tests/test_x.py::test_y   # why that test would run now, without starting Python
forktest --fork-from imported            # override where workers fork from (auto, collected, imported, spawn)
forktest -k billing -x       # anything forktest does not know passes through to pytest

forktest watch               # rerun affected tests on every save, safety net in the background
forktest daemon start        # keep warm zygotes behind a unix socket; plain `forktest` uses them
forktest daemon status
forktest daemon stop
```

Modes forktest has nothing to add to run plain pytest with the same
arguments: `--collect-only`, `--fixtures`, `--fixtures-per-test`, `--markers`,
`--version`, `-h`, `--pdb`, `--trace`, `--setup-plan`, `--setup-only`,
`--setup-show`, `--lf`, `--ff`, `--sw` and `--stepwise-skip`.

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

Exit codes follow pytest: 0 all selected tests passed, 1 a test failed or a
miss was found, 2 collection failed, 4 pytest rejected the arguments, 5
nothing was collected at all.

## How it works

**Fingerprints.** Rust parses every `.py` file with ruff's parser into a map
from function qualname, spelled as CPython's `co_qualname`, to a hash of its
definition with docstrings removed, plus a `<module>` hash of the file with
every function body replaced by `...`. The hashes come from ruff's
position-free AST, so moving, reformatting or re-commenting code changes
nothing. Editing a constant, a signature, an import or a decorator changes
the `<module>` hash. Parsing pandas' `frame.py` takes about 4 ms, against
108 ms with Python's `ast`. Each file's fingerprint, imports and stat are
kept in `.forktest/forktest.db`, and a file is read again only when its stat
changes.

**Selection before Python.** A test runs when it is new, failed last time,
or any recorded dependency hash differs from the current one. Each distinct
dependency is checked once, and only the changed ones' rows of a key to
tests index are read, so the work grows with the change, not the suite. A
test also runs when its file's static import closure reaches a project file
it did not record. Each selected test carries one reason: `config_change`,
`new`, `last_failed`, `coverage`, `fixture`, `file_read` or `import_graph`.
`--why` prints them. On pandas' `frame` tests, Rust's selection picks the
same tests for the same reasons as the Python version's in every scenario
`bench/selcheck.py` tries, in 28 to 38 ms against 66 to 182 ms.

**Collecting only what runs.** The store keeps, per set of pytest arguments,
each test file's nodeids, its content hash, a digest of its import closure,
and the functions and data files its collection used. When nothing is
selected and nothing changed, Python never starts. Otherwise only the files
holding selected tests are collected, and within them only the selected
functions and classes, unless a file changed in a way that can change its
tests. A new or removed test file, a config change, `--all` and
`--verify-selection` collect everything.

**Where workers fork from.** Rust counts each Python process's OS threads
from outside it (`proc_pidinfo` on macOS, `/proc` on Linux), which sees the
native threads `threading.active_count()` cannot. L0 is an interpreter with
pytest and the project's third-party packages imported. L1 is forked from it
and collects. Workers fork from L1 when it still has one thread. When
collection started threads (JAX building arrays at import, a conftest
starting a thread), workers fork from L0 instead and each collects its own
files, as xdist workers do. When even a bare interpreter has threads, each
worker is a fresh interpreter. A preimport that starts threads, or that pulls
in a plugin pytest must import itself, is dropped from L0. The answer is
stored, so the next run of a JAX suite skips L1 when the inventory already
names every test to run.

**Execution and recording.** Selected tests are grouped by module, assigned
to workers by longest processing time from what each test cost last time,
and each worker runs its share in pytest's collection order through the
`pytest_runtest_protocol` hook, so plugins that wrap it still apply. A
`sys.monitoring` `PY_START` hook records which project functions ran, an
`open` audit hook records which project data files were read, and fixture
definitions are added statically. Workers send each distinct function name
once and each test's hits as packed ids; Rust turns them into dependency
sets and skips the write when a test's set is unchanged.

**macOS.** A forked child cannot reach notifyd, so libc rereads the time zone
file on every `localtime()`: `datetime.now()` takes 168 µs in a forked
worker against 0.32 µs in its parent. forktest loads a small interposer into
the test interpreter that keeps libc's cached zone, which brings it to
0.26 µs, and removes it from the environment before any test runs. It also
sets `OBJC_DISABLE_INITIALIZE_FORK_SAFETY=YES` for its Python processes.

**Daemon.** `forktest daemon start` keeps L0 warm and, when the fork point
allows, a fully collected L1. A file watcher (FSEvents, inotify) tells it
which files changed, so a request trusts every other fingerprint without a
stat. A change to a file L1 imported or read, a test file, or the set of
`.py` files rebuilds L1 in the background, and a request that arrives
meanwhile collects only what it needs in an L1 of its own. A change to
`pyproject.toml` or a lockfile restarts L0. `forktest watch` is the same
daemon in the foreground: each change cancels the run in progress, runs the
affected tests, then runs the unselected ones at low priority so a selection
miss is reported rather than hidden.

## Measured

On a 12-core Apple Silicon laptop, Python 3.12 for the demo and 3.14 for
attrs and pandas, on 2026-10-30. Each table interleaves its sides round by
round after a warm-up round, and reports the median wall-clock time of one
CLI invocation with the range after it. Other work kept the load average
between 3 and 6 for most rounds and up to 10 at the end of the pandas ones.
Every forktest run reported the same pass, fail, skip and xfail counts as
pytest. The Python version ran from a checkout of `main` (1fe3bc5).

### The demo

`examples/demo`, 16 tests, one of them sleeping 0.25 s. The edit inserts a
statement into `round_vat` and selects the 7 tests that run it; the daemon
rows flip `+ 0` on its return instead. 5 rounds.

| scenario | pytest | Python forktest | Rust forktest |
|---|---|---|---|
| one-function edit | 1.32 s, whole suite | 0.211 s (0.204 to 0.216) | 0.150 s (0.149 to 0.159) |
| rerun, nothing changed | | 0.176 s (0.175 to 0.183) | 0.009 s (0.009 to 0.009) |
| `--all` | 1.32 s (1.32 to 1.33) | 0.468 s (0.467 to 0.470) | 0.407 s (0.402 to 0.409) |
| edit through the daemon, zygote rebuilt | | 59.8 ms (59.0 to 62.4) | 27.3 ms (25.1 to 27.8) |
| edit through the daemon, during the rebuild | | 172.3 ms (167.6 to 175.6) | 91.2 ms (88.7 to 95.7) |

A rerun with nothing changed starts no Python. The edit run is bound by
starting Python and `import pytest` (about 70 ms) and the 0.25 s sleeps bound
`--all`.

### attrs

[attrs](https://github.com/python-attrs/attrs) at c8bb9bf: 1,413 tests, 2 of
which fail in a shallow clone. The edit adds a line to
`_InstanceOfValidator.__repr__` and selects the 12 tests that run it plus the
2 that failed last time. The rerun with nothing changed reruns those 2. 5
rounds.

| scenario | pytest | pytest -n auto | Python forktest | Rust forktest |
|---|---|---|---|---|
| one-function edit | 5.86 s, whole suite | 2.86 s, whole suite | 0.599 s (0.592 to 0.633) | 0.321 s (0.317 to 0.325) |
| rerun, nothing changed | | | 0.566 s (0.557 to 0.578) | 0.307 s (0.302 to 0.311) |
| `--all` | 5.86 s (5.81 to 6.13) | 2.86 s (2.77 to 2.90) | 1.303 s (1.25 to 1.44) | 1.193 s (1.17 to 1.22) |

No `--all` run can beat its longest test,
`TestMetadata::test_not_none_metadata`, which takes 0.42 s by pytest's count.

### pandas

[pandas](https://github.com/pandas-dev/pandas) at c32acd3, an editable meson
build, pytest 9.2, without pyarrow. The slice is `pandas/tests/frame`: 16,876
tests, of which 41 fail in this environment. The edit adds a line to
`describe_ndframe` and selects the 55 tests that run it plus the 41 that
failed last time. 3 rounds.

| scenario | pytest -n auto | Python forktest | Rust forktest |
|---|---|---|---|
| one-function edit, 96 tests | 12.56 s, whole slice | 3.71 s (3.71 to 3.98) | 2.02 s (1.93 to 2.04) |
| rerun, nothing changed, 41 tests | | 3.67 s (3.65 to 3.90) | 2.04 s (1.90 to 2.09) |
| `--all` | 12.56 s (12.02 to 12.62) | 8.46 s (8.02 to 8.63) | 7.47 s (6.82 to 7.56) |

Without the daemon, every run pays `import pandas` (0.9 s), since pandas is
the project, and the 41 failing tests format their tracebacks. Collection is
narrowed to the selected functions: 280 items instead of the 9,037 their files
hold.

### JAX

`examples/jaxdemo`: 8 tests, and `tests/test_eager.py` builds JAX arrays at
import, which starts 61 native threads during collection. The Python forktest
forked 4 workers from that process and all 4 hung in XLA's compiler. 5
rounds, Python 3.12, jax 0.12.

| scenario | pytest | Rust forktest |
|---|---|---|
| one-function edit, 7 tests | 0.930 s, whole suite | 0.656 s (0.621 to 0.699) |
| `--all` | 0.930 s (0.917 to 0.950) | 0.830 s (0.785 to 0.852) |

Workers fork from L0, before collection, and each starts its own JAX
backend. No run printed JAX's fork warning. `import pytest, jax` alone takes
0.36 s of the edit run.

### Other projects

`bench/corpus.py` runs pytest, `pytest -n auto`, forktest, rtest and rpytest
on more projects (more-itertools, click, flask, rich, FastAPI, Django REST
framework) and checks every count against pytest; `bench/args_parity.py`
checks exit codes and counts for 127 argument sets across five projects.

## Limitations, honestly

- **Linux is untested.** Everything above ran on macOS. The Linux paths
  (`/proc` thread counts, inotify) compile and are written for it, but have
  not run. The daemon watches the project recursively, and inotify's watch
  limit may need raising on large trees.
- **First runs record everything.** A run with an empty store collects and
  records every test, which on attrs is slower than `pytest -n auto`. Every
  later run is faster.
- **Each worker builds session fixtures itself.** A session fixture runs once
  per worker rather than once per run, and a test that mutates one does not
  affect tests in other workers. `--strict-pytest` gives exact single-process
  semantics.
- **A function behind a cache is recorded by the test that filled it.** With
  `functools.lru_cache`, or a JAX function reached through a jit trace another
  test already compiled, later tests in the same worker never run it, so an
  edit to it does not select them. `--verify-selection` and the watch safety
  net report such a miss.
- **A file's stat is trusted.** A file whose `mtime_ns`, `ctime_ns` and size
  match the stored ones is not read again. A file read within 2 s of its
  last change is read again next time, as git does for racily clean files.
- **New test files outside `python_files`** (doctest targets, plugin-collected
  YAML) are found by the next full collection, not the next partial one.
- **pytest's rootdir** is approximated without Python. When pytest picks a
  different one, forktest warns and selection may be off.
- **Captured output is not shown.** Failure events carry the assertion
  message and the project frames, not the captured stdout of the test.
- **An edit saved during a run** can be recorded against the wrong content.
  The safety net and the next full run catch it.
- **Same-second, same-size edits can run stale bytecode.** CPython's `.pyc`
  check uses the source mtime in whole seconds and the size, as plain pytest
  does. forktest's selection is content-based and still picks the right tests.
- pytest-xdist is replaced, not supported. `forktest -h` shows pytest's help;
  forktest's own options are listed above.
- **Windows** is not supported.

## Development

```bash
cargo build --release
cargo test --release                                  # unit tests and the process layer against real interpreters
.venv/bin/python -m pytest tests                      # CLI, daemon, watch, JAX and interpreter tests on the binary
FORKTEST_PY_SRC=../ft-py-ref/src cargo test parity    # fingerprints and import closures against the Python version
python bench/bench_ab.py ROOT TESTPATH EDIT_FILE ANCHOR_LINE ROUNDS   # pytest, Python and Rust forktest, interleaved
python bench/bench_daemon.py SOURCE EDIT_FILE OLD NEW EXPECT_FAILED ROUNDS
```

`FORKTEST_BIN` names the binary the Python tests and benches run, and
`FORKTEST_PY_SRC` a checkout of `main`'s `src` for the Python version.
`RUST_PLAN.md` is the design. `DESIGN.md` is the original product spec.
