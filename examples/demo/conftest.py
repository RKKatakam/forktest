import time
from decimal import Decimal

import pytest

from billing.invoice import Invoice


@pytest.fixture(scope="session")
def ledger():
    time.sleep(0.02)
    opening = Invoice()
    opening.add("opening balance", 1, Decimal("100"))
    return [opening]
