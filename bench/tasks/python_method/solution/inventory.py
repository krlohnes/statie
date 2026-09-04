class Inventory:
    """A simple name -> quantity stock tracker."""

    def __init__(self):
        self._stock = {}

    def add(self, name, qty):
        """Adds `qty` units of `name` to stock."""
        self._stock[name] = self._stock.get(name, 0) + qty

    def remove(self, name, qty):
        """Removes `qty` units of `name` from stock.

        Raises `KeyError` if `name` is not in stock, and `ValueError` if
        `qty` exceeds the current stock for `name`. If the remaining
        quantity reaches zero, the key is removed entirely.
        """
        if name not in self._stock:
            raise KeyError(name)
        current = self._stock[name]
        if qty > current:
            raise ValueError(
                f"cannot remove {qty} of {name!r}; only {current} in stock"
            )
        remaining = current - qty
        if remaining == 0:
            del self._stock[name]
        else:
            self._stock[name] = remaining

    def total(self):
        """Returns the total number of units across all items."""
        return sum(self._stock.values())
