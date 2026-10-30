"""Rust selection against the Python forktest's, on a project's real recorded store.

usage: python bench/selcheck.py ROOT ROUNDS

ROOT holds a Python forktest store (.forktest/store.db), the .venv that recorded it,
and a clean git tree. The
store is copied, converted once into a Rust store, then for every scenario below
the edit is applied, both selectors run ROUNDS times interleaved over every
recorded test, and the edit is reverted with git checkout. Both sides run with no
fixture locations, and with the import edges the live session saved, since its
sys.modules map is not persisted.
"""

import json
import os
import shutil
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(sys.argv[1])
ROUNDS = int(sys.argv[2])
WORK = Path("/tmp/selcheck")
REPO = Path(__file__).resolve().parent.parent
PY = str(ROOT / ".venv/bin/python")
PY_SRC = "/Users/katakam/repos/ft-py-ref/src"
RS_TEST = sorted((REPO / "target/release/deps").glob("forktest-*"), key=os.path.getmtime)[-1]

SCENARIOS = [
    ("no edit", []),
    (
        "body of DataFrame.memory_usage",
        [("pandas/core/frame.py", "        return result\n\n    def transpose(", "        result = result\n        return result\n\n    def transpose(")],
    ),
    (
        "docstring of DataFrame.memory_usage",
        [("pandas/core/frame.py", "Return the memory usage of each column in bytes.", "Return the memory use of each column, in bytes.")],
    ),
    (
        "reformat + comment in DataFrame.round",
        [
            (
                "pandas/core/frame.py",
                "        self, decimals: int | dict[IndexLabel, int] | Series = 0, *args, **kwargs\n    ) -> DataFrame:",
                "        self,\n        decimals: int | dict[IndexLabel, int] | Series = 0,\n        *args,\n        **kwargs,\n    ) -> DataFrame:  # reformatted",
            )
        ],
    ),
    (
        "module-level statement in nanops.py",
        [("pandas/core/nanops.py", "def set_use_bottleneck(v: bool = True) -> None:", "_forktest_edit = 1\n\n\ndef set_use_bottleneck(v: bool = True) -> None:")],
    ),
    (
        "body of one test in test_round.py",
        [("pandas/tests/frame/methods/test_round.py", "        # GH#12600\n", "        # GH#12600\n        _forktest_edit = 1\n")],
    ),
    (
        "fixture body in tests/frame/conftest.py",
        [("pandas/tests/frame/conftest.py", 'index=pd.date_range("2000-01-01", periods=10, freq="B"),', 'index=pd.date_range("2000-01-01", periods=10, freq="B").copy(),')],
    ),
]

PY_SELECT = r"""
import functools, json, os, sys, time
from forktest.fingerprint import Fingerprints
from forktest.selection import ImportGraph, select, dep_changed
from forktest.store import Store
root, store_path, graph_path, out_path, mode = sys.argv[1:6]
t = time.perf_counter()
store = Store(store_path)
fps = Fingerprints(root, store.file_infos())
view = store.view()
graph = ImportGraph(root, fps)
# The live session resolved these edges with its sys.modules map, which the recorded module sets reflect.
graph._direct = {p: frozenset(t) for p, (_, _, t) in json.loads(store.meta_get("import_graph"))["nodes"].items()}
current = functools.cache(fps.of)
if mode == "stale":
    json.dump([i for i, d in view.keys.items() if dep_changed(d, current)], open(out_path, "w"))
    sys.exit()
reasons, changed = select(sorted(view.tests), view, current, graph)
ms = (time.perf_counter() - t) * 1000
json.dump({"ms": ms, "reasons": {n: str(r) for n, r in reasons.items()}, "changed": changed}, open(out_path, "w"))
"""


def py(mode: str) -> dict | list:
    out = WORK / "py_out.json"
    subprocess.run(
        [PY, "-c", PY_SELECT, str(ROOT), str(WORK / "py/store.db"), str(WORK / "py_graph.json"), str(out), mode],
        env={**os.environ, "PYTHONPATH": PY_SRC},
        check=True,
    )
    return json.loads(out.read_text())


def rs(test: str) -> dict:
    out = WORK / "rs_out.json"
    env = {
        **os.environ,
        "FT_ROOT": str(ROOT),
        "FT_PY_STORE": str(WORK / "py/store.db"),
        "FT_STALE": str(WORK / "stale.json"),
        "FT_RS_STORE": str(WORK / "rs"),
        "FT_OUT": str(out),
    }
    proc = subprocess.run([str(RS_TEST), "--ignored", "--exact", test, "--nocapture"], env=env, capture_output=True, text=True)
    if proc.returncode:
        sys.exit(proc.stdout + proc.stderr)
    return json.loads(out.read_text()) if test.endswith("select_on_real_store") else proc.stderr.strip().splitlines()[-1]


def git_clean() -> None:
    subprocess.run(["git", "checkout", "--", "pandas"], cwd=ROOT, check=True)


def main() -> None:
    git_clean()
    shutil.rmtree(WORK, ignore_errors=True)
    (WORK / "py").mkdir(parents=True)
    shutil.copy(ROOT / ".forktest/store.db", WORK / "py/store.db")
    (WORK / "stale.json").write_text(json.dumps(py("stale")))
    print(rs("select::real_store::convert_python_store"))
    py("select")
    rs("select::real_store::select_on_real_store")
    print(f"{'scenario':42} {'python':>7} {'rust':>7}  same set  same reasons  python ms  rust ms")
    for name, edits in SCENARIOS:
        try:
            for rel, old, new in edits:
                path = ROOT / rel
                text = path.read_text()
                assert text.count(old) == 1, f"{rel}: anchor appears {text.count(old)} times"
                path.write_text(text.replace(old, new))
            py_ms, rs_ms = [], []
            for _ in range(ROUNDS):
                p = py("select")
                r = rs("select::real_store::select_on_real_store")
                py_ms.append(p["ms"])
                rs_ms.append(r["ms"])
        finally:
            git_clean()
        same_set = p["reasons"].keys() == r["reasons"].keys()
        same_reasons = p["reasons"] == r["reasons"]
        print(
            f"{name:42} {len(p['reasons']):>7} {len(r['reasons']):>7}  {str(same_set):8}  {str(same_reasons):12}"
            f"  {statistics.median(py_ms):9.0f}  {statistics.median(rs_ms):7.1f}"
        )
        if not same_reasons:
            diff = sorted(set(p["reasons"].items()) ^ set(r["reasons"].items()))
            print("   differs:", diff[:10], f"({len(diff)} entries)")
        counts: dict[str, int] = {}
        for reason in r["reasons"].values():
            counts[reason] = counts.get(reason, 0) + 1
        print(f"   rust reasons {counts}, changed {r['changed']}; python changed {p['changed']}")


main()
