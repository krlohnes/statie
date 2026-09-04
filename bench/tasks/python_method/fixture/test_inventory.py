import unittest

from inventory import Inventory


class TestInventory(unittest.TestCase):
    def test_add_and_total(self):
        inv = Inventory()
        inv.add("widget", 5)
        inv.add("gadget", 2)
        self.assertEqual(inv.total(), 7)

    def test_remove_reduces_quantity(self):
        inv = Inventory()
        inv.add("widget", 5)
        inv.remove("widget", 2)
        self.assertEqual(inv.total(), 3)

    def test_remove_unknown_name_raises_key_error(self):
        inv = Inventory()
        with self.assertRaises(KeyError):
            inv.remove("missing", 1)

    def test_remove_more_than_stock_raises_value_error(self):
        inv = Inventory()
        inv.add("widget", 5)
        with self.assertRaises(ValueError):
            inv.remove("widget", 10)

    def test_remove_to_zero_deletes_key(self):
        inv = Inventory()
        inv.add("widget", 5)
        inv.remove("widget", 5)
        with self.assertRaises(KeyError):
            inv.remove("widget", 1)


if __name__ == "__main__":
    unittest.main()
