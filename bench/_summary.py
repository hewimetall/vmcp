"""Side-by-side clean summary of A vs C with errors filtered."""

import json
from pathlib import Path
from statistics import mean

ROOT = Path(__file__).resolve().parent


def load(p: Path):
    rows = [json.loads(line) for line in p.open(encoding="utf-8")]
    ok = [r for r in rows if r["error"] is None and not r["truncated"]]
    return rows, ok


def main():
    A_all, A = load(ROOT / "results/A_current.jsonl")
    C_all, C = load(ROOT / "results/C_noguidance.jsonl")

    print(f"A_current:    {len(A)}/{len(A_all)} OK"
          f" ({100 * (len(A_all) - len(A)) / len(A_all):.0f}% errors)")
    print(f"C_noguidance: {len(C)}/{len(C_all)} OK"
          f" ({100 * (len(C_all) - len(C)) / len(C_all):.0f}% errors)")
    print()

    cols = (
        f"{'task_id':28} | "
        f"{'A: calls':>9} {'%1':>5} {'tok':>6} | "
        f"{'C: calls':>9} {'%1':>5} {'tok':>6} | "
        f"{'d calls':>8}"
    )
    print(cols)
    print("-" * len(cols))

    tasks = sorted({r["task_id"] for r in A_all})
    for t in tasks:
        a = [r for r in A if r["task_id"] == t]
        c = [r for r in C if r["task_id"] == t]
        if not a or not c:
            continue
        ac = mean(r["tool_call_count"] for r in a)
        cc = mean(r["tool_call_count"] for r in c)
        as_ = 100 * sum(1 for r in a if r["tool_call_count"] == 1) / len(a)
        cs = 100 * sum(1 for r in c if r["tool_call_count"] == 1) / len(c)
        at = mean(r["total_tokens"] for r in a)
        ct = mean(r["total_tokens"] for r in c)
        print(
            f"{t:28} | "
            f"{ac:9.2f} {as_:4.0f}% {at:6.0f} | "
            f"{cc:9.2f} {cs:4.0f}% {ct:6.0f} | "
            f"{cc - ac:+8.2f}"
        )

    print("-" * len(cols))
    print(
        f"{'OVERALL':28} | "
        f"{mean(r['tool_call_count'] for r in A):9.2f} "
        f"{100 * sum(1 for r in A if r['tool_call_count'] == 1) / len(A):4.0f}% "
        f"{mean(r['total_tokens'] for r in A):6.0f} | "
        f"{mean(r['tool_call_count'] for r in C):9.2f} "
        f"{100 * sum(1 for r in C if r['tool_call_count'] == 1) / len(C):4.0f}% "
        f"{mean(r['total_tokens'] for r in C):6.0f} | "
        f"{mean(r['tool_call_count'] for r in C) - mean(r['tool_call_count'] for r in A):+8.2f}"
    )


if __name__ == "__main__":
    main()
