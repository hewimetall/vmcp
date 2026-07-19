"""Tiny demo module for filesystem + agent-lsp smoke."""


def greet(name: str) -> str:
    """Return a friendly greeting."""
    return f"Hello, {name}!"


if __name__ == "__main__":
    print(greet("stand"))
