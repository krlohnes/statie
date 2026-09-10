"""Computes order totals from parsed line items.

Consumes `LineItem`s from `parser.py` (whose `unit_price_cents` is always
whole cents) and produces totals that `formatter.py` renders for the
receipt.
"""

from formatter import format_amount

TAX_RATE = 0.08  # 8%


def line_total_cents(item):
    """Returns the total price (in cents) for one line item: quantity
    times unit price."""
    return item.quantity * item.unit_price_cents


def subtotal_cents(items):
    """Returns the sum of every line item's total, in cents."""
    return sum(line_total_cents(item) for item in items)


def tax_cents(subtotal):
    """Returns the tax owed (in cents) on a subtotal given in cents."""
    return round(subtotal * TAX_RATE)


def order_total_cents(items):
    """Returns the full order total in cents: subtotal plus tax."""
    sub = subtotal_cents(items)
    return sub + tax_cents(sub)


def format_order_total(items):
    """Returns the order total as a display string, e.g. "$97.20"."""
    total = order_total_cents(items)
    return format_amount(total)
