class Inventory:
    """A simple name -> quantity stock tracker."""

    def __init__(self):
        self._stock = {}

    def add(self, name, qty):
        """Adds `qty` units of `name` to stock."""
        self._stock[name] = self._stock.get(name, 0) + qty

    def total(self):
        """Returns the total number of units across all items."""
        return sum(self._stock.values())
