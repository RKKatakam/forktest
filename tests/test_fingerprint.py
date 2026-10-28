import textwrap

import pytest

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
