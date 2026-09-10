import unittest

from parser import LineItem
from pricing import (
    format_order_total,
    line_total_cents,
    order_total_cents,
    subtotal_cents,
    tax_cents,
)


class TestPricing(unittest.TestCase):
    def test_line_total_cents_multiplies_quantity_by_unit_price(self):
        item = LineItem("widget", 3, 250)
        self.assertEqual(line_total_cents(item), 750)

    def test_subtotal_cents_sums_all_lines(self):
        items = [LineItem("a", 1, 100), LineItem("b", 2, 50)]
        # 100 + (2 * 50) = 200
        self.assertEqual(subtotal_cents(items), 200)

    def test_tax_cents_is_eight_percent_of_subtotal_in_cents(self):
        # Subtotal stays in cents throughout: tax on 1000 cents is 80
        # cents, not 8 cents (which would be the result of computing tax on
        # a value that had already been divided by 100).
        self.assertEqual(tax_cents(1000), 80)

    def test_order_total_cents_is_subtotal_plus_tax_in_cents(self):
        items = [LineItem("widget", 2, 4500)]
        # subtotal 9000 cents, tax 720 cents -> 9720 cents
        self.assertEqual(order_total_cents(items), 9720)

    def test_format_order_total_renders_dollars_from_cents(self):
        items = [LineItem("widget", 2, 4500)]
        self.assertEqual(format_order_total(items), "$97.20")

    def test_format_order_total_for_a_single_cheap_item(self):
        items = [LineItem("pen", 1, 150)]
        self.assertEqual(format_order_total(items), "$1.62")


if __name__ == "__main__":
    unittest.main()
