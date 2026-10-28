# forktest: a fast, pytest-compatible test runner for the agent era

*`forktest` is a working name.*

## Pitch

Edit a file and see the results of exactly the tests it affects in **under 300 ms**. Your tests still run on real pytest semantics, so your existing suite, fixtures, and plugins work unchanged. It targets the same experience uv gave `pip`.

## Problem

On a typical mid-sized Python project, running tests after a one-line change spends most of its time on work that isn't your tests:

| Phase | What happens | Typical cost |
|---|---|---|
| Interpreter + imports | Re-import Django, pandas, SQLAlchemy... | 2–10 s |
| Collection | Import *every* test module, run every `conftest.py` | 1–5 s |
| Session fixtures | Rebuild database schema, app factories | 1–10 s |
| Execution | Run 4,000 tests when 12 were affected | 10 s – minutes |
| Parallelism | `xdist` workers each pay all of the above again | multiplier |

*The costs above are illustrative ranges, not measurements. Producing real numbers is part of the benchmark plan.*

Humans put up with this a few times an hour. Agents hit it hundreds of times per task, and it becomes their main bottleneck.

## Goals

1. **Sub-second edit-to-result** for incremental runs on real projects.
2. **Drop-in compatibility:** existing pytest suites, fixtures, markers, `conftest.py` hierarchies, and most plugins work unchanged.
3. **Correct selection:** it never silently skips a test that should have run, and a background safety net catches misses.
4. **First-class agent interface:** structured, token-efficient output and a daemon an agent can query.
5. **Faster full runs too:** warm forking makes `--all` faster than `pytest -n auto`.

## Non-goals (for v1)

- Replacing pytest's assertion rewriting, fixture model, or plugin API. It uses them.
- Windows support beyond a fallback mode, since Windows has no `fork()`.
- Non-Python tests.
- Distributed or remote execution. Maelstrom covers that space.

---

## User experience

```bash
uv tool install forktest

forktest                 # run tests affected by uncommitted changes
forktest watch           # rerun affected tests on every save
forktest --all           # full suite, warm-forked and parallel
forktest --since main    # affected by everything since branching from main
forktest --json          # structured output for agents and CI
forktest -k billing      # pytest selection flags pass through
```

Human output:

```
forktest · 3 files changed · 14 of 4,212 tests selected (0.04s)
  ✓ tests/billing/test_invoice.py   9 passed
  ✗ tests/billing/test_tax.py::test_vat_rounding
      billing/tax.py:42  assert round_vat(Decimal("10.005")) == Decimal("10.01")
      got Decimal("10.00")
  ✓ tests/api/test_checkout.py       4 passed
1 failed, 13 passed in 0.23s · background full run: queued
```

Agent output (`--json`, one JSON object per line):

```json
{"event":"selection","changed":["billing/tax.py"],"selected":14,"total":4212,"reasons":{"coverage":11,"import_graph":2,"last_failed":1}}
{"event":"fail","test":"tests/billing/test_tax.py::test_vat_rounding","file":"billing/tax.py","line":42,"message":"assert Decimal('10.00') == Decimal('10.01')","trace":[{"file":"billing/tax.py","line":42,"fn":"round_vat"}]}
{"event":"done","passed":13,"failed":1,"duration_ms":231}
```

Tracebacks include only frames from the project's own code. Each selection carries the *reason* a test was chosen, so an agent can trust the result or override it.

---

## Architecture

```
┌──────────────────────────── forktest daemon (Rust) ────────────────────────────┐
│                                                                                │
│  File watcher ──► Change detector ──► Selector ──► Scheduler ──► Reporter      │
│   (notify)        (hash + AST diff)    (coverage    (order,        (human /    │
│                                        + imports    batching,      JSONL /     │
│                                        + rules)     streaming)     JUnit)      │
│                                                                                │
│  Static collector (ruff_python_parser)    Store (SQLite: coverage, durations,  │
│                                                  collection cache, history)    │
│                                                                                │
│  Zygote manager ─────────────────── Unix socket IPC ───────────────────┐       │
└────────────────────────────────────────────────────────────────────────┼───────┘
                                                                         │
     ┌───────────────────────── Python side (small shim) ────────────────▼───┐
     │  L0 zygote: interpreter + third-party deps imported                   │
     │    └─fork─► L1 zygote: + project modules + conftest.py imported       │
     │               └─fork─► L2 zygote: + session fixtures built            │
     │                          └─fork─► worker ─► runs test item(s) ─► exit │
     └───────────────────────────────────────────────────────────────────────┘
```

**Rust side:** a daemon per project or worktree, plus a thin CLI that talks to it. Built on `tokio` for async, `notify` for file watching, `ruff_python_parser`, `rusqlite`, and `serde`.

**Python side:** a small shim that loads as a pytest plugin inside the zygote. Its jobs are building the session, forking on command, running items through pytest's own `runtestprotocol`, and streaming results back.

### Lifecycle of one incremental run

1. The watcher sees `billing/tax.py` saved. The change detector compares its hash and AST to the last version and finds that `round_vat` changed.
2. The selector queries the store for tests whose recorded coverage includes `round_vat`, adds static import-graph dependents, last-failed tests, and tests in changed test files.
3. The zygote manager checks whether any warm layer had imported `billing/tax.py`. If yes, it rebuilds from the lowest affected layer. If no, the layers are still valid.
4. The scheduler orders the tests (last-failed first, then by shortest duration), groups them into batches, and sends fork commands.
5. Workers fork from L2, run their items, stream results over the socket, and exit.
6. The reporter streams output as results arrive. A background full run is queued as a safety net.

---

## Key mechanisms

### 1. Static collection
- Parse test files in Rust without importing them. Find `test_*` functions, `Test*` classes, and `@pytest.mark.parametrize` decorators whose arguments are literal values.
- Cache results per file content hash, so unchanged files cost nothing.
- **Fallback:** dynamic parametrization, `pytest_generate_tests`, and collection hooks from plugins get collected inside the warm zygote instead. That's still fast, because the imports are already done.
- Static results are always confirmed against pytest's real collection in the zygote. Static collection makes it fast. pytest's collection decides what's correct.

### 2. Layered zygotes
| Layer | Contains | Rebuilt when |
|---|---|---|
| **L0** | Interpreter + third-party packages | Lockfile or venv changes |
| **L1** | Project modules imported by `conftest.py` and test modules | A project file it imported changes |
| **L2** | Session-scoped fixtures built | L1 is rebuilt, or a fixture definition changes |

- Each layer records exactly which modules it imported, with file paths and content hashes taken from `sys.modules`.
- A change invalidates only layers that imported the changed file. Most edits touch application code that L0 never imported, so the expensive layer survives almost every edit.
- Call `gc.freeze()` before forking, so CPython's garbage collector doesn't write to every object and defeat copy-on-write sharing.
- **Multi-agent bonus:** worktrees with the same lockfile can share one L0 zygote on the same machine. Ten agents get warm dependencies for the price of one.

### 3. Fork execution
- A worker is `os.fork()` from L2. It runs one item or a batch through pytest's own machinery, reports, and exits. Teardown is just the process exiting.
- **Batching:** forking costs about 1 ms. For suites with thousands of tiny tests, fork once per module-sized batch. The scheduler chooses batch sizes using duration history.
- **A semantic difference to be honest about:** a session fixture built in L2 is *copied* into each worker. If one test mutates it, other tests don't see the change. That usually fixes real test-pollution bugs, but it is a behavior change. `--strict-pytest` runs a single shared session for exact semantics.

### 4. Fork safety (the hardest operational problem)
- **Threads:** before forking a layer, check `threading.enumerate()` and the OS thread count. A zygote with live threads isn't safe to fork, and Python 3.12+ warns about it.
- **Sockets and connections:** a database connection opened in a zygote would be shared by every child, which corrupts it. The shim provides `forktest_prefork` and `forktest_postfork` hooks, plus built-in adapters that close connections before forking and reconnect after for **pytest-django, SQLAlchemy, redis, and httpx**.
- **Known-unsafe state:** an initialized CUDA context, gRPC channels, and some async event loops. Detect them, then automatically demote that suite to *spawn mode*, where workers start fresh but test selection still works.
- **macOS:** forking after Apple system frameworks have initialized can crash the process, which is why `multiprocessing` defaults to spawn there. Linux is the primary target. macOS gets detection, `OBJC_DISABLE_INITIALIZE_FORK_SAFETY` handling, and the spawn fallback.

### 5. Test selection
Selection combines four signals, any of which can select a test:

| Signal | How | Catches |
|---|---|---|
| **Coverage map** | Record functions each test executed with `sys.monitoring` (PEP 669, Python 3.12+, low overhead), keyed by function-body hash, as pytest-testmon does with blocks | Most code dependencies |
| **File reads** | `sys.addaudithook` on `open` events, recorded per test | Config files, fixtures on disk, templates, data files |
| **Static import graph** | Built from the Rust parser | New tests, code paths without coverage yet, module-level code changes |
| **Rules** | Last failed, changed test files, `conftest.py` changes, lockfile changes (which trigger a full run) | Everything structural |

- Only changed *functions* select tests, not whole changed files. Editing a docstring in a 3,000-line module shouldn't run 800 tests.
- Python versions before 3.12 fall back to a `sys.settrace`-based recorder, which works but is slower.

### 6. Safety net
- After every incremental run, a background full run (lower priority, cancellable) checks selection. If a test that wasn't selected fails, forktest reports it loudly and records why it was missed.
- Running `--all` in CI also keeps the coverage map fresh.
- Track the selection miss rate as a core quality metric. The target is effectively zero.

### 7. Reporters
- **Human:** compact, with failures first and streamed as results arrive.
- **JSONL:** for agents, as shown above.
- **JUnit XML:** so CI dashboards keep working.
- **MCP server:** an agent can call `run_affected()`, `run(tests)`, and `why_selected(test)` directly.

---

## Compatibility

| Feature | Status |
|---|---|
| Fixtures (all scopes), `conftest.py` hierarchy, parametrize, markers, `-k` and `-m` | ✅ Uses pytest's own machinery |
| Assertion rewriting | ✅ Done by pytest in the zygote |
| Common plugins (pytest-asyncio, pytest-mock, pytest-cov, pytest-django, hypothesis, pytest-randomly) | ✅ Tested, with adapters where needed |
| pytest-xdist | ⛔ Replaced: forktest *is* the parallelism |
| Plugins that assume one process for the whole session | ⚠️ Works with `--strict-pytest` |
| Mutation of session fixtures across tests | ⚠️ Semantic difference (see above) |

**Risk:** the shim depends on pytest internals (`_pytest.*`), which aren't public API. Mitigate by pinning tested pytest versions, running a CI matrix across pytest releases, and keeping the shim small.

---

## Performance targets

| Metric | Target |
|---|---|
| Edit to first result (1-file change, warm) | **< 300 ms** |
| Static collection of 10k tests (cached) | < 50 ms |
| Static collection of 10k tests (cold) | < 300 ms |
| Fork overhead per worker | < 2 ms |
| Full suite vs `pytest -n auto` | ≥ 2× faster |
| Selection miss rate | ≈ 0, measured continuously by the safety net |

## Benchmark plan

Use real open-source projects that use pytest, for example **pydantic, httpx, FastAPI, SQLAlchemy, rich, attrs, and pandas**. pandas is the stress test because of its size. Django itself uses its own test runner, so use a pytest-django application instead.

For each project, replay the last 50 real commits from its git history, one at a time, and measure:
1. Time from the edit to the first result and to the last affected result: `pytest` vs `pytest --testmon` vs forktest
2. Tests selected compared to tests that actually fail when everything runs, i.e. selection precision and recall
3. Full-suite time: `pytest -n auto` vs `forktest --all`

Publish the methodology and the scripts. A credible, reproducible benchmark table was a big part of uv's launch.

---

## Roadmap

| Milestone | Scope | Exit criterion |
|---|---|---|
| **M0: proof of concept** (a weekend) | Pure-Python prototype: one zygote, fork per test, real pytest items | Measured speedup on one real repo |
| **M1: the Rust core** | Daemon, CLI, IPC, static collection, `watch` | Equal to M0 or faster, with a usable CLI |
| **M2: layered zygotes** | L0/L1/L2 layers with import tracking and invalidation | Most edits avoid rebuilding L0 |
| **M3: selection** | `sys.monitoring` coverage, audit-hook file tracking, import graph, safety net | Miss rate is effectively zero on the benchmark replay |
| **M4: compatibility** | Fork-safety detection, database and connection adapters, macOS handling, plugin matrix | All benchmark repos pass with an unmodified suite |
| **M5: launch** | JSONL and MCP interfaces, JUnit, docs, public benchmark | Public release + blog post with the numbers |

---

## Risks and open questions

1. **Fork-unsafe state in real suites** could push many projects into spawn mode. M4 has to find out how common that is early.
2. **pytest internals churn:** you're coupled to `_pytest`. Is a small upstream contribution (a stable "run this item in this process" hook) feasible?
3. **Selection trust:** one well-publicized miss can kill adoption. The safety net and the `reasons` field exist for this.
4. **Competition:** rtest, rstest, rpytest, and tezt are all early. Your edge is the combination: warm layered zygotes, function-level selection, and agent-native output. None of them does all three.
5. **Monetization (if it ever matters):** a hosted shared coverage map and selection service for CI and agent fleets is the obvious paid layer, the way Astral has kept its core tools open.

---

## Success looks like

- Someone runs `uvx forktest` on an unmodified repo and gets correct results 20–50× faster on incremental runs.
- Agent frameworks call it as their default test tool.
- The benchmark post makes the rounds the same way uv's did.
