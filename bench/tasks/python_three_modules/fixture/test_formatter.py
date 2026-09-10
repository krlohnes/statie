import unittest

from formatter import format_amount, format_cents


class TestFormatter(unittest.TestCase):
    def test_format_cents_formats_whole_dollars(self):
        self.assertEqual(format_cents(9720), "$97.20")

    def test_format_cents_handles_negative(self):
        self.assertEqual(format_cents(-150), "-$1.50")

    def test_format_cents_pads_single_digit_cents(self):
        self.assertEqual(format_cents(105), "$1.05")

    def test_format_amount_formats_dollars(self):
        self.assertEqual(format_amount(97.2), "$97.20")


if __name__ == "__main__":
    unittest.main()
