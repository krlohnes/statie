"""Builds a printable receipt from raw order lines."""

from parser import parse_order
from pricing import format_order_total, order_total_cents, subtotal_cents


def build_receipt(raw_lines):
    """Parses `raw_lines` and returns a multi-line receipt string with a
    subtotal, an itemized line per item, and a total line."""
    items = parse_order(raw_lines)
    lines = []
    for item in items:
        lines.append(f"{item.name} x{item.quantity}")
    lines.append(f"Subtotal (cents): {subtotal_cents(items)}")
    lines.append(f"Total: {format_order_total(items)}")
    return "\n".join(lines)
