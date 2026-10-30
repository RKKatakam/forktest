# forktest in Rust: implementation plan

Goal: replace the Python forktest (on `main`, the reference implementation) with a Rust binary that is faster, stays a drop-in pytest replacement, and never forks a process that has threads, so suites using JAX, torch or anything else with native thread pools run correctly.

The Python version stays on `main` as the behavioral reference. Its CLI and daemon tests were ported to `tests/e2e`, which runs the binary, and the parity tests and `bench/bench_ab.py` run the Python version from a checkout of `main` named by `FORKTEST_PY_SRC`.

## Why the rewrite changes the process model

Measured on this machine (Python 3.12, jax 0.12, macOS):

- `import jax` leaves the process at 1 OS thread. The first `jnp.ones(3)` takes it to 62. `threading.active_count()` stays at 1 throughout.
- The Python forktest checks only `threading.active_count()`. On `examples/jaxdemo`, whose `tests/test_eager.py` builds arrays at import, it forks 4 workers from a zygote with 62 native threads. All 4 hang inside `xla::CompileCpuExecutableInternal` waiting for a thread pool that did not survive `fork()`. Plain pytest runs the same 8 tests in 0.8 s.

So the thread check has to count OS threads, from outside the process, and there has to be somewhere safe to fork from when collection starts threads.

## Definition of done

1. `forktest` is one Rust binary. `forktest [pytest args]` accepts every pytest argument, and pass/fail/skip/xfail/xpass counts match `pytest` with the same arguments on `examples/demo`, `examples/jaxdemo`, attrs and pandas' `pandas/tests/frame`.
2. Exit codes match pytest: 0, 1 (failures or a miss), 2 (collection errors), 5 (nothing collected). Modes forktest cannot serve (`--collect-only`, `--fixtures`, `--markers`, `--version`, `-h`, `--pdb`, `--trace`, `--setup-plan`, `--setup-only`, `--lf`, `--ff`, `--sw`) exec plain pytest with the same arguments.
3. On `examples/jaxdemo` with `-n 4`: every run completes, results match pytest, no worker forks from a process with more than one OS thread, and JAX never prints its fork warning.
4. Every behavior in `main`'s `tests/test_cli_demo.py` and `tests/test_daemon.py` holds against the Rust binary (the ported suite in `tests/e2e` runs it through `FORKTEST_BIN`).
5. Faster than the Python version, interleaved on the same machine, for: a cold edit run, a no-op run, `--all`, and a daemon edit run, on the demo, attrs and pandas' frame slice.

## Architecture

```
forktest (Rust)                                              Python (embedded shim, forktest_shim.py)
-----------------------------------------------------------  ----------------------------------------------------
cli ─► root ─► fingerprint (ruff parser, rayon) ─► graph     L0  python shim.py l0: import pytest + preimports
            ─► store (SQLite) ─► select ─► schedule              │ fork on "collect"
            ─► zygote (process layer, unix socket server)        ├─► L1: pytest.main(args) restricted to files,
            ─► report (human, jsonl, junit)                       │        reports items; forks workers on "run"
daemon/watch: same pipeline, long-lived, notify-based watcher    │          └─► worker: runs its batch
                                                                  └─► worker (Imported mode): pytest.main(args)
                                                                        restricted to its batch's files, runs it
```

### Fork points (the thread rule)

A fork point is a process forked from. `ForkPoint` is a state per session:

| Fork point | Workers fork from | Each worker collects | Chosen when |
|---|---|---|---|
| `Collected` | L1 | nothing | L1 has 1 OS thread after collection (beyond tolerated pools) and `threading.active_count() == 1` |
| `Imported` | L0 | its own batch's files | L1 has extra threads (JAX arrays at import, CUDA init, a conftest thread) |
| `Spawn` | nothing, fresh `python shim.py worker` per batch | its own batch's files | L0 itself has extra threads with no preimports |

- Rust counts a child's OS threads with `proc_pidinfo(PROC_PIDTASKINFO)` on macOS and `/proc/<pid>/status` on Linux (`threads.rs`). Python-level threads are reported by the shim.
- Tolerated pools. On Linux, OpenBLAS starts its pool when numpy loads and makes it fork-safe with `pthread_atfork`. The shim imports preimports one top-level module at a time and measures the OS thread delta after each. A delta from a module in `FORK_SAFE_POOLS` (`numpy`, `scipy`) is tolerated and reported as `tolerated_threads`. A delta from any other module drops that module from the preimport list and L0 restarts once without it. Every fork-point check is `os_threads <= 1 + tolerated_threads`.
- The chosen fork point and the dropped preimports are persisted in store meta per args key, so the next run goes straight to the right layer.
- Override: `--fork-from auto|collected|imported|spawn`, or the ini option `forktest_fork_from` (the shim registers it with `parser.addini`).

### Partial collection (the main speedup after an edit)

On pandas' frame slice, `pytest --collect-only` alone is 2.2 s of the Python forktest's 2.9 s edit run. Rust selects before Python starts, so L1 only collects the files that hold selected tests.

- The store keeps an inventory per args key: for each collected test file, its nodeids after deselection (`-k`, `-m`), the file's content hash, a digest of its static import closure (`<module>` hashes of every file `graph.closure(file)` reaches, conftests included), and the functions and data files recorded while that file was collected (parametrize helpers, `pytest_generate_tests`).
- A file is re-collected when its content hash, closure digest or recorded collection deps changed, or it had a collection error last time.
- A full collection runs when there is no inventory for the args key, a file matching `python_files` or a `conftest.py` was added or removed, a config file changed, or the mode needs every test (`--all`, `--verify-selection`).
- The shim restricts collection with a `tryfirst` `pytest_ignore_collect`: a directory with no target beneath it, and a file that is not a target (other than `__init__.py`), are ignored. User arguments, rootdir and conftest discovery are untouched.
- A file collected only because it holds selected tests (not re-collected) is narrowed further: a `tryfirst` `pytest_pycollect_makeitem` returns no items for names that are not on a selected test's nodeid path (module-level functions and classes, then methods, nested classes in turn, parameters stripped). Every parametrization of a named function is made, and Rust keeps the selected ones. Its stored inventory row stands, since the collection saw only part of it. When a selected test is missing from a narrowed collection (a plugin that names items apart from their functions), the files are collected again whole.
- When workers collect their own files (an `imported` or `spawn` fork point) and no file needs re-collecting, a partial run starts no L1: the selected nodeids come from the inventory. Each worker sends its config's `maxfail` before its first result, and a nodeid it does not find is reported as an error.
- Nothing selected and nothing to re-collect means Python never starts.
- The daemon rebuilds its warm L1 in the background: a request that arrives meanwhile forks its own L1 from L0 for the files it needs, and the rebuild goes on. A file reported changed while the rebuild collects stales it once it is done, unless its content matches the hash taken before L1 started (importing pandas from a meson editable install rewrites `_version_meson.py` with the same bytes).

### Execution and recording

Unchanged from the Python version where it was right: items run through `item.ihook.pytest_runtest_protocol(item, nextitem)` with `nextitem` chained inside a batch so the last item tears down every scope; `sys.monitoring` `PY_START` returning `DISABLE` plus `restart_events()` per test; an `open` audit hook for non-Python reads; static fixture locations from `item._fixtureinfo`; the capture manager re-initialized in each forked worker; `gc.freeze()` around forks.

What moves to Rust: turning hits into dependencies, digests, the store write, scheduling and reporting. Workers send `(relpath, qualname)` names once per connection and each test's hits as a packed `u32` array.

## Data shapes

Rust (`src/`):

- `RelPath`: root-relative, `/`-separated `Arc<str>`.
- `Hash128`: `u128` from xxh3-128, stored as a 16-byte BLOB.
- `Qual`: `Module` (`<module>`), `File` (`<file>`) or `Func(Arc<str>)`, a CPython `co_qualname`.
- `Dep { path: RelPath, qual: Qual, hash: Hash128 }`.
- `FileInfo { sha: Hash128, stat: Stat, kind: Python { fingerprint, imports } | Unparseable | Other }`. A fingerprint maps qualname to hash, with `<module>` the hash of the file with every function body replaced by `...`. Docstrings are removed first. Hashes come from `ruff_python_ast::comparable`, so formatting, comments and parentheses do not count. Duplicate qualnames (`@overload`) keep the last definition, as the Python version's dict did.
- `Reason`: `all`, `config_change`, `new`, `last_failed`, `coverage`, `fixture`, `file_read`, `import_graph`, in that priority.
- `Event`: `Selection`, `TestResult`, `CollectError`, `Miss`, `Verified`, `Done`, with the same JSON as the Python version.

Store (`.forktest/forktest.db`, its own file so the Python version's `store.db` stays usable for A/B runs):

```
tests(id, nodeid UNIQUE, outcome, cost REAL, last_run REAL, modules INTEGER, digest BLOB)
test_deps(test_id PRIMARY KEY, deps BLOB)            -- packed u32 key ids
dep_keys(id, path, qualname, hash BLOB, UNIQUE(path, qualname, hash))
key_tests(key_id PRIMARY KEY, tests BLOB)            -- packed u32 test ids, patched on every write
module_sets(id, paths TEXT UNIQUE)
files(path PRIMARY KEY, sha BLOB, mtime_ns, ctime_ns, size, info BLOB)
inventory(args_key, path, sha BLOB, closure BLOB, deps TEXT, nodeids TEXT, error INTEGER, skipped INTEGER,
          PRIMARY KEY(args_key, path))
meta(key PRIMARY KEY, value)                         -- preimports, sys.modules map, fixture locations,
                                                     -- import graph state, fork point per args key
```

## Rust ⇄ Python protocol

Rust listens on a unix socket (`.forktest/run-<pid>.sock`, or under `TMPDIR` when that path exceeds 96 bytes) and passes its path in `FORKTEST_SOCK`. Every Python process (L0, L1, each worker) connects once and says hello. A frame is `u32 BE total length`, `u32 BE json length`, the JSON, then an optional binary blob filling the rest.

| From | Message | Meaning |
|---|---|---|
| L0 | `hello {role: "l0", pid, python, os_threads}` | |
| Rust | `config {preimport: [...], root}` | |
| L0 | `ready {os_threads, py_threads, tolerated_threads, dropped: [...]}` | preimports done |
| Rust | `collect {cid, args, files: null \| [...]}` | L0 forks L1 |
| L1 | `hello {role: "l1", pid, cid}` | |
| L1 | `collected {items: [{nodeid, path}], errors, skips, maxfail, rootdir, python_files, fork_from, modules, third_party, fixtures, per_file: {path: {names, reads}}, py_threads}` | after `pytest_collection_modifyitems` |
| L1 | `failed {code}` | pytest exited before the run loop |
| Rust | `run {rid, batches: [[nodeid]], nice}` | L1 forks one worker per batch |
| Rust | `run_workers {rid, args, batches: [{nodeids, files}], nice}` | to L0, Imported fork point |
| worker | `hello {role: "worker", pid, rid, index}` | |
| worker | `result {nodeid, outcome, duration, cost, file, line, message, trace, names: [[rel, qual]], reads: [rel]}` + blob of u32 name ids | |
| worker | `collect_error`, `done`, `crash {message}` | |
| L0/L1 | `exited {rid, index, status}` | after `waitpid` |
| Rust | `exit` | |

## Module map and ownership

| File | Owns | Unit |
|---|---|---|
| `src/main.rs`, `src/cli.rs` | argument parsing, passthrough detection, dispatch | U3 |
| `src/root.rs` | rootdir discovery, config files, excluded dirs | U1 |
| `src/fingerprint.rs` | `analyze`, `FileInfo`, stat-trusted `Fingerprints` cache, parallel prefetch | U1 |
| `src/graph.rs` | import resolution, closures, persisted edges | U1 |
| `src/store.rs` | schema, `StoreView`, `record`, `forget`, inventory, meta | U1 |
| `src/select.rs` | `select`, `explain`, `dep_changed`, `classify`, re-collection decision | U1 |
| `src/schedule.rs` | longest-processing-time batches with module and class splitting | U1 |
| `src/events.rs` | event types and JSON | scaffold |
| `src/proto.rs`, `src/threads.rs`, `src/python.rs`, `src/zygote.rs`, `shim/forktest_shim.py` | process layer | U2 |
| `src/run.rs`, `src/report.rs` | the pipeline from request to events, reporters, exit codes | U3 |
| `src/daemon.rs` | daemon, watch, safety net | U4 |

Units U1 and U2 run in parallel in separate worktrees. U3 joins them. U4 follows U3. U5 is measurement and the README.

## Verification harness

- Unit tests per Rust module (`cargo test`).
- `tests/` (pytest) drives the binary named by `FORKTEST_BIN` on scratch copies of `examples/demo` and `examples/jaxdemo`.
- `bench/bench_ab.py` runs pytest, the Python forktest (from a `main` worktree) and the Rust binary interleaved on the same project and prints medians and ranges.
