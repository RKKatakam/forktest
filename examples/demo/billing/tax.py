import json
import pathlib
from decimal import ROUND_HALF_UP, Decimal

import config

RATES_FILE = pathlib.Path(__file__).resolve().parent.parent / "data" / "rates.json"


def round_vat(amount: Decimal) -> Decimal:
    """Round to the configured number of places, half up."""
    quantum = Decimal(1).scaleb(-config.ROUNDING_PLACES)
    return amount.quantize(quantum, rounding=ROUND_HALF_UP)


def vat_for(net: Decimal, rate: Decimal | None = None) -> Decimal:
    rate = config.VAT_RATE if rate is None else rate
    return round_vat(net * rate)


def load_rates() -> dict[str, Decimal]:
    with open(RATES_FILE) as f:
        return {country: Decimal(rate) for country, rate in json.load(f).items()}


def rate_for(country: str) -> Decimal:
    return load_rates()[country]
