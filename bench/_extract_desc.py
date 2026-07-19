"""One-shot: extract query_graphql tool description from vmcp-server/src/lib.rs
and write to descriptions/HEAD.txt. Handles Rust raw strings (`r#"..."#`),
line-continuation, and common escape sequences.

`HEAD.txt` always mirrors the description currently shipped in source.
Historical snapshots (`A_current.txt`, `A_v2.txt`, …) are kept separately
under `descriptions/` and tied to specific benchmark runs.

Run after every change to the tool description in lib.rs:
  uv run python bench/_extract_desc.py
"""

import pathlib
import re

SRC = pathlib.Path(__file__).resolve().parents[1] / "crates/vmcp-server/src/lib.rs"
OUT = pathlib.Path(__file__).resolve().parent / "descriptions/HEAD.txt"


def extract(text: str) -> str:
    """Pull the first `description = ...` literal out of a Rust source.
    Handles two forms:

      1. Raw string  `description = r#"..."#`  — verbatim, no escapes.
      2. Escaped     `description = "..."`     — apply Rust line-continuation
                                                  (`\\<LF><ws>` drops) plus
                                                  `\\n`, `\\"`, `\\t`, `\\\\`.
    """
    # Try raw string first (current source uses r#"..."#).
    m = re.search(r'description\s*=\s*r#"(.*?)"#', text, re.DOTALL)
    if m:
        return m.group(1)

    # Fall back to escaped literal.
    m = re.search(r'description\s*=\s*"((?:\\.|[^"\\])*)"', text, re.DOTALL)
    if not m:
        raise SystemExit("description block not found")
    raw = m.group(1)

    # Rust line continuation: `\<LF><leading whitespace>` is dropped.
    raw = re.sub(r"\\\n\s*", "", raw)

    out = []
    i = 0
    while i < len(raw):
        c = raw[i]
        if c == "\\" and i + 1 < len(raw):
            nxt = raw[i + 1]
            if nxt == "n":
                out.append("\n")
            elif nxt == "t":
                out.append("\t")
            elif nxt == '"':
                out.append('"')
            elif nxt == "\\":
                out.append("\\")
            else:
                out.append(c + nxt)  # unknown — keep as-is
            i += 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def main() -> None:
    text = SRC.read_text(encoding="utf-8")
    desc = extract(text)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(desc, encoding="utf-8")
    line_count = desc.count("\n") + 1
    print(f"wrote {OUT} ({len(desc)} chars, {line_count} lines)")
    print("--- first 250 chars ---")
    print(desc[:250])
    print("--- last 250 chars ---")
    print(desc[-250:])


if __name__ == "__main__":
    main()
