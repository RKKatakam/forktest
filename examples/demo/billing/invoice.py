from dataclasses import dataclass, field
from decimal import Decimal

from billing.tax import vat_for
from config import CURRENCY


@dataclass
class Line:
    description: str
    quantity: int
    unit_price: Decimal

    def net(self) -> Decimal:
        return self.unit_price * self.quantity


@dataclass
class Invoice:
    lines: list[Line] = field(default_factory=list)

    def add(self, description: str, quantity: int, unit_price: Decimal) -> Line:
        line = Line(description, quantity, unit_price)
        self.lines.append(line)
        return line

    def net(self) -> Decimal:
        return sum((line.net() for line in self.lines), Decimal(0))

    def vat(self) -> Decimal:
        return vat_for(self.net())

    def total(self) -> Decimal:
        return self.net() + self.vat()

    def describe(self) -> str:
        return f"{len(self.lines)} lines, total {self.total()} {CURRENCY}"
