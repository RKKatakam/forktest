import json
from decimal import Decimal

from billing.invoice import Invoice


class TestInvoice:
    def test_net(self):
        inv = Invoice()
        inv.add("widget", 3, Decimal("2.50"))
        assert inv.net() == Decimal("7.50")

    def test_total_includes_vat(self):
        inv = Invoice()
        inv.add("widget", 1, Decimal("100"))
        assert inv.total() == Decimal("120.00")

    def test_describe_currency(self):
        inv = Invoice()
        inv.add("widget", 1, Decimal("10"))
        assert inv.describe() == "1 lines, total 12.00 EUR"


def test_ledger_has_opening_balance(ledger):
    assert Decimal(json.loads(ledger.read_text())["opening"]) == Decimal("100")
