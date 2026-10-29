from types import SimpleNamespace

from forktest.zygote import schedule


def items(*nodeids):
    return [SimpleNamespace(nodeid=n) for n in nodeids]


def where(plan):
    return {i.nodeid: k for k, batch in enumerate(plan) for i in batch}


def test_a_module_heavier_than_an_even_share_is_split_along_class_boundaries():
    heavy = [f"tests/test_heavy.py::Test{c}::test_{k}" for c in "ABCD" for k in range(5)]
    light = [f"tests/test_light_{m}.py::test_{k}" for m in range(4) for k in range(3)]
    durations = {n: 0.1 for n in heavy} | {n: 0.01 for n in light}
    plan = schedule(items(*heavy, *light), durations.__getitem__, set(), workers=4)
    worker = where(plan)

    loads = [sum(durations[i.nodeid] for i in batch) for batch in plan]
    assert len(plan) == 4 and max(loads) <= 0.6
    for c in "ABCD":
        assert len({worker[f"tests/test_heavy.py::Test{c}::test_{k}"] for k in range(5)}) == 1, c
    for m in range(4):
        assert len({worker[f"tests/test_light_{m}.py::test_{k}"] for k in range(3)}) == 1, m


def test_one_oversized_class_is_split_into_consecutive_runs():
    nodeids = [f"t.py::TestBig::test_{k}" for k in range(8)]
    plan = schedule(items(*nodeids), lambda n: 0.1, set(), workers=4)
    assert sorted(len(b) for b in plan) == [2, 2, 2, 2]
    for batch in plan:
        ks = [int(i.nodeid.rsplit("_", 1)[1]) for i in batch]
        assert ks == list(range(ks[0], ks[0] + len(ks)))


def test_unknown_durations_still_spread_a_single_module_over_the_workers():
    plan = schedule(items(*(f"t.py::test_{k}" for k in range(6))), lambda n: 0.01, set(), workers=3)
    assert [len(b) for b in plan] == [2, 2, 2]


def test_modules_that_fit_stay_whole_and_failed_tests_run_first():
    nodeids = [f"a.py::test_{k}" for k in range(3)] + [f"b.py::test_{k}" for k in range(3)]
    plan = schedule(items(*nodeids), lambda n: 0.01, {"b.py::test_2"}, workers=2)
    assert sorted(sorted(i.nodeid[0] for i in b) for b in plan) == [["a"] * 3, ["b"] * 3]
    assert plan[0][0].nodeid.startswith("b.py")
