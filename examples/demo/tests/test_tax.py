from decimal import Decimal

import pytest

from billing.tax import rate_for, round_vat, vat_for


@pytest.mark.parametrize(
    ("raw", "expected"),
    [("10.005", "10.01"), ("10.004", "10.00"), ("0.125", "0.13")],
)
def test_round_vat(raw, expected):
    assert round_vat(Decimal(raw)) == Decimal(expected)


def test_vat_for_default_rate():
    assert vat_for(Decimal("100")) == Decimal("20.00")


def test_vat_for_explicit_rate():
    assert vat_for(Decimal("100"), Decimal("0.1")) == Decimal("10.00")


def test_rate_for_country():
    assert rate_for("SE") == Decimal("0.25")
