from config import CURRENCY, VAT_RATE


def test_currency():
    assert CURRENCY == "EUR"


def test_vat_rate_is_a_fraction():
    assert 0 < VAT_RATE < 1
