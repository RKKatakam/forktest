import json

from forktest.store import Dep, Deps, deps_digest
from forktest.worker import DepDecoder, DepEncoder


class Item:
    def __init__(self, nodeid):
        self.nodeid = nodeid


def test_deps_cross_the_pipe_once_per_worker_and_an_unchanged_set_sends_nothing():
    a = [Dep("a.py", "f", "1"), Dep("s.py", "<module>", "2")]
    b = [Dep("b.py", "g", "3"), Dep("s.py", "<module>", "2")]
    sets = {"t::a": a, "t::b": b, "t::c": a}
    encoder = DepEncoder(lambda item, hits, reads: sorted(sets[item.nodeid]), {"t::c": Deps.of(a).digest})
    decoder = DepDecoder()
    wire = [json.loads(json.dumps(encoder.encode(Item(n), set(), set()))) for n in ("t::a", "t::b", "t::c")]
    assert [decoder.decode(m) for m in wire] == [Deps.of(a), Deps.of(b), None]
    assert sum(len(m.get("keys", ())) for m in wire) == 3


def test_the_digest_ignores_order_and_sees_any_change_to_the_set():
    deps = [Dep(f"m{k}.py", "f", str(k)) for k in range(50)]
    assert deps_digest(deps) == deps_digest(deps[::-1])
    assert deps_digest(deps) != deps_digest(deps[1:])
    assert deps_digest(deps) != deps_digest([*deps[1:], Dep("m0.py", "f", "changed")])
