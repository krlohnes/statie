import unittest

from parser import parse_line, parse_order


class TestParser(unittest.TestCase):
    def test_parse_line_reads_name_quantity_price(self):
        item = parse_line("widget,2,4500")
        self.assertEqual(item.name, "widget")
        self.assertEqual(item.quantity, 2)
        self.assertEqual(item.unit_price_cents, 4500)

    def test_parse_line_strips_whitespace(self):
        item = parse_line(" widget , 2 , 4500 ")
        self.assertEqual(item.name, "widget")

    def test_parse_line_rejects_malformed_line(self):
        with self.assertRaises(ValueError):
            parse_line("widget,2")

    def test_parse_order_skips_blank_lines(self):
        items = parse_order(["widget,2,4500", "", "gadget,1,1099"])
        self.assertEqual(len(items), 2)


if __name__ == "__main__":
    unittest.main()
