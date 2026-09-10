"""Parses raw order lines into line items.

Each raw line has the form "name,quantity,price", where `price` is given in
whole cents (e.g. "1050" means $10.50). This module is the single source of
truth for that unit: everywhere else in this package, a "price" or "amount"
field that came from here is in cents unless explicitly converted.
"""


class LineItem:
    """One parsed order line. `unit_price_cents` is an integer number of
    cents -- never a float, and never dollars."""

    def __init__(self, name, quantity, unit_price_cents):
        self.name = name
        self.quantity = quantity
        self.unit_price_cents = unit_price_cents

    def __repr__(self):
        return (
            f"LineItem(name={self.name!r}, quantity={self.quantity}, "
            f"unit_price_cents={self.unit_price_cents})"
        )


def parse_line(raw):
    """Parses one "name,quantity,price" line into a `LineItem`.

    `price` in the raw text is written as whole cents, e.g. "1050" for
    $10.50. Raises `ValueError` if the line is malformed.
    """
    parts = raw.split(",")
    if len(parts) != 3:
        raise ValueError(f"expected 'name,quantity,price', got {raw!r}")
    name, quantity_str, price_str = parts
    name = name.strip()
    try:
        quantity = int(quantity_str.strip())
        unit_price_cents = int(price_str.strip())
    except ValueError as exc:
        raise ValueError(f"expected integers for quantity/price in {raw!r}") from exc
    return LineItem(name, quantity, unit_price_cents)


def parse_order(raw_lines):
    """Parses each line in `raw_lines` into a `LineItem`, skipping blank
    lines."""
    return [parse_line(line) for line in raw_lines if line.strip()]
