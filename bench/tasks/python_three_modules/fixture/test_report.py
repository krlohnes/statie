import unittest

from report import build_receipt


class TestReport(unittest.TestCase):
    def test_receipt_total_is_in_dollars_not_cents(self):
        # 2 x "widget" @ 4500 cents = 9000 cents subtotal, 8% tax = 720
        # cents -> total 9720 cents = $97.20. The receipt must show dollars,
        # not cents formatted as if they were already dollars.
        receipt = build_receipt(["widget,2,4500"])
        self.assertIn("Total: $97.20", receipt)
        self.assertNotIn("$9720.00", receipt)

    def test_receipt_lists_each_item(self):
        receipt = build_receipt(["widget,2,4500", "gadget,1,1099"])
        self.assertIn("widget x2", receipt)
        self.assertIn("gadget x1", receipt)

    def test_receipt_subtotal_is_reported_in_cents(self):
        receipt = build_receipt(["widget,2,4500"])
        self.assertIn("Subtotal (cents): 9000", receipt)

    def test_receipt_total_for_small_order(self):
        # 1 x "pen" @ 150 cents = 150 cents subtotal, 8% tax = round(12) =
        # 12 cents -> total 162 cents = $1.62.
        receipt = build_receipt(["pen,1,150"])
        self.assertIn("Total: $1.62", receipt)


if __name__ == "__main__":
    unittest.main()
