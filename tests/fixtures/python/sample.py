"""Sample Python fixture for tree-sitter parsing tests."""


class PythonClass:
    """A sample Python class."""

    def __init__(self, value):
        self.value = value

    def calculate(self, x, y):
        """Calculate something."""
        return x + y


def top_level_func():
    """A top-level function."""
    return "hello"


def another_function():
    """Another function."""
    pass
