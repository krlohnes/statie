"""Formats money amounts for display.

There are two entry points on purpose: `format_cents` is the correct one to
use for any amount that came from `parser.py` or `pricing.py`'s totals
(both of which work in cents). `format_amount` is a legacy helper that
treats its argument as whole/fractional *dollars*; it exists only for
old call sites that have not been migrated and should not be used with a
cents value.
"""


def format_cents(cents):
    """Formats a whole number of cents as a dollar string, e.g. 1050 ->
    "$10.50"."""
    sign = "-" if cents < 0 else ""
    cents = abs(cents)
    dollars, remainder = divmod(cents, 100)
    return f"{sign}${dollars}.{remainder:02d}"


def format_amount(amount):
    """Legacy: formats `amount`, already in dollars (a float or int), as a
    dollar string, e.g. 10.5 -> "$10.50". Do not pass a cents value here."""
    sign = "-" if amount < 0 else ""
    amount = abs(amount)
    return f"{sign}${amount:.2f}"
