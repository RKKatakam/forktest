import os
import textwrap
import time

import pytest

import forktest.fingerprint as fingerprint_module
from forktest.fingerprint import (
    FILE,
    MODULE,
    Fingerprints,
    ImportStmt,
    fingerprint,
    imports,
    normalize_qualname,
)

SRC = textwrap.dedent(
    '''
    """Module docstring."""
    import os
    from decimal import Decimal

    RATE = Decimal("0.2")


    def top(x):
        """Doc."""
        def inner(y):
            return y + 1
        return inner(x) * 2


    class Billing:
        places = 2

        def total(self, n):
            return n * RATE

        @staticmethod
        def helper():
            return 1


    if os.name == "posix":
        def guarded():
            return "posix"
    '''
)


def test_qualnames_match_co_qualname():
    assert set(fingerprint(SRC)) == {
        MODULE,
        "top",
        "top.<locals>.inner",
        "Billing.total",
        "Billing.helper",
        "guarded",
    }


def test_function_body_edit_changes_only_that_function_and_its_parents():
    before = fingerprint(SRC)
    after = fingerprint(SRC.replace("return y + 1", "return y + 2"))
    changed = {q for q in before if before[q] != after[q]}
    assert changed == {"top", "top.<locals>.inner"}


def test_docstring_reformat_and_move_do_not_change_hashes():
    edited = SRC.replace('"""Doc."""', '"""A different doc."""').replace(
        '"""Module docstring."""', '"""Other."""'
    )
    assert fingerprint(edited) == fingerprint(SRC)
    moved = SRC.replace("\n\n    class Billing", "\n\n\n\n    class Billing")
    reformatted = SRC.replace("return n * RATE", "return (n * RATE)")
    assert fingerprint(moved) == fingerprint(SRC)
    assert fingerprint(reformatted) == fingerprint(SRC)


@pytest.mark.parametrize(
    "edit",
    [
        ('RATE = Decimal("0.2")', 'RATE = Decimal("0.25")'),
        ("places = 2", "places = 3"),
        ("def top(x):", "def top(x, y=1):"),
        ("import os", "import os, sys"),
        ("@staticmethod", "@classmethod"),
    ],
)
def test_module_hash_covers_constants_signatures_imports_decorators(edit):
    before = fingerprint(SRC)
    after = fingerprint(SRC.replace(*edit))
    assert before[MODULE] != after[MODULE]


def test_module_hash_ignores_function_bodies():
    after = fingerprint(SRC.replace("return n * RATE", "return n * RATE * 2"))
    assert fingerprint(SRC)[MODULE] == after[MODULE]


def test_normalize_qualname_folds_synthetic_scopes_into_their_def():
    fp = fingerprint(SRC)
    assert normalize_qualname("top.<locals>.<lambda>", fp) == "top"
    assert normalize_qualname("top.<locals>.inner.<locals>.<genexpr>", fp) == "top.<locals>.inner"
    assert normalize_qualname("Billing", fp) == MODULE
    assert normalize_qualname("<lambda>", fp) == MODULE
    assert normalize_qualname("removed_function", fp) == MODULE
    assert normalize_qualname("Billing.total", fp) == "Billing.total"


def test_imports_lists_every_import_including_nested_and_relative():
    src = textwrap.dedent(
        """
        import a.b
        from c import d, e
        from . import f
        from ..g import h
        def lazy():
            import z
        """
    )
    assert imports(src) == [
        ImportStmt("a.b", 0, ()),
        ImportStmt("c", 0, ("d", "e")),
        ImportStmt("", 1, ("f",)),
        ImportStmt("g", 2, ("h",)),
        ImportStmt("z", 0, ()),
    ]


def test_fingerprints_cache_tracks_edits_and_non_python_files(tmp_path):
    (tmp_path / "m.py").write_text("def f():\n    return 1\n")
    (tmp_path / "data.json").write_text("{}")
    fps = Fingerprints(str(tmp_path))
    first = fps.of("m.py")
    assert set(first) == {MODULE, "f"}
    assert fps.of("m.py") is first
    (tmp_path / "m.py").write_text("def f():\n    return 2\n")
    assert fps.of("m.py")["f"] != first["f"]
    assert set(fps.of("data.json")) == {FILE}
    assert fps.of("missing.py") is None
    (tmp_path / "broken.py").write_text("def (:\n")
    assert set(fps.of("broken.py")) == {MODULE}
    assert fps.imports_of("m.py") == []


def test_known_infos_are_reused_only_while_the_content_matches(tmp_path):
    (tmp_path / "m.py").write_text("import os\n\ndef f():\n    return 1\n")
    cold = Fingerprints(str(tmp_path))
    info = cold.info("m.py")
    assert cold.fresh == {"m.py": info} and info.imports == [ImportStmt("os", 0, ())]
    planted = info._replace(fingerprint={MODULE: "from the store"})
    warm = Fingerprints(str(tmp_path), {"m.py": planted})
    assert warm.of("m.py") == {MODULE: "from the store"} and warm.fresh == {}
    (tmp_path / "m.py").write_text("def f():\n    return 2\n")
    stale = Fingerprints(str(tmp_path), {"m.py": planted})
    assert set(stale.of("m.py")) == {MODULE, "f"} and stale.imports_of("m.py") == []
    assert stale.fresh["m.py"].sha != info.sha


def test_a_later_process_trusts_an_unchanged_stat_and_rereads_after_any_write(tmp_path, monkeypatch):
    monkeypatch.setattr(fingerprint_module, "RACY_NS", 0)
    m = tmp_path / "m.py"
    m.write_text("def f():\n    return 1\n")
    info = Fingerprints(str(tmp_path)).info("m.py")
    planted = info._replace(fingerprint={MODULE: "planted"})
    assert Fingerprints(str(tmp_path), {"m.py": planted}).of("m.py") == {MODULE: "planted"}, "same stat, not read"

    before = os.stat(m)
    time.sleep(0.05)  # Linux stamps ctime at timer-tick granularity
    m.write_text("def f():\n    return 2\n")
    os.utime(m, ns=(before.st_atime_ns, before.st_mtime_ns))
    after = os.stat(m)
    assert (after.st_mtime_ns, after.st_size) == (before.st_mtime_ns, before.st_size)
    assert Fingerprints(str(tmp_path), {"m.py": planted}).of("m.py")["f"] != info.fingerprint["f"]


def test_a_file_read_in_the_tick_it_was_written_is_reread_even_if_its_stat_repeats(tmp_path, monkeypatch):
    m = tmp_path / "m.py"
    m.write_text("def f():\n    return 1\n")
    real_stat, frozen = os.stat, os.stat(m)
    monkeypatch.setattr(os, "stat", lambda p, *a, **k: frozen if p == str(m) else real_stat(p, *a, **k))
    first = Fingerprints(str(tmp_path)).info("m.py")
    m.write_text("def f():\n    return 2\n")
    assert Fingerprints(str(tmp_path), {"m.py": first}).of("m.py")["f"] != first.fingerprint["f"]
