"""Compare two benchmark runs side-by-side.

Usage:
  uv run python analyze.py results/A.jsonl results/C.jsonl
"""

from __future__ import annotations

import pathlib

import pandas as pd
import typer
from rich.console import Console
from rich.table import Table

app = typer.Typer(add_completion=False)


def _load(path: pathlib.Path) -> pd.DataFrame:
    df = pd.read_json(path, lines=True)
    if df.empty:
        raise SystemExit(f"empty: {path}")
    return df


def _agg(df: pd.DataFrame) -> pd.DataFrame:
    g = df.groupby("task_id").agg(
        n=("tool_call_count", "size"),
        calls_mean=("tool_call_count", "mean"),
        calls_p50=("tool_call_count", "median"),
        single_rate=("tool_call_count", lambda s: (s == 1).mean() * 100),
        tokens_mean=("total_tokens", "mean"),
        wall_mean=("wall_ms", "mean"),
    )
    return g


@app.command()
def main(
    a: pathlib.Path = typer.Argument(..., help="JSONL — control / baseline."),
    b: pathlib.Path = typer.Argument(..., help="JSONL — variant under test."),
) -> None:
    console = Console()
    da = _load(a)
    db = _load(b)
    ga = _agg(da)
    gb = _agg(db)

    tag_a = da["tag"].iloc[0]
    tag_b = db["tag"].iloc[0]

    common = sorted(set(ga.index) | set(gb.index))

    table = Table(title=f"A={tag_a} ({a.name})  vs  B={tag_b} ({b.name})")
    table.add_column("task_id", style="cyan", no_wrap=True)
    table.add_column("n", justify="right")
    table.add_column("calls A", justify="right")
    table.add_column("calls B", justify="right")
    table.add_column("Δ calls", justify="right", style="bold")
    table.add_column("%single A", justify="right")
    table.add_column("%single B", justify="right")
    table.add_column("tokens A", justify="right")
    table.add_column("tokens B", justify="right")
    table.add_column("Δ tokens", justify="right")

    for t in common:
        ra = ga.loc[t] if t in ga.index else None
        rb = gb.loc[t] if t in gb.index else None
        ca = ra["calls_mean"] if ra is not None else float("nan")
        cb = rb["calls_mean"] if rb is not None else float("nan")
        sa = ra["single_rate"] if ra is not None else float("nan")
        sb = rb["single_rate"] if rb is not None else float("nan")
        ta_ = ra["tokens_mean"] if ra is not None else float("nan")
        tb_ = rb["tokens_mean"] if rb is not None else float("nan")
        d_calls = (cb - ca) if (ra is not None and rb is not None) else float("nan")
        d_tokens = (tb_ - ta_) if (ra is not None and rb is not None) else float("nan")
        n = int(ra["n"]) if ra is not None else int(rb["n"]) if rb is not None else 0
        table.add_row(
            str(t),
            str(n),
            f"{ca:.2f}",
            f"{cb:.2f}",
            f"{d_calls:+.2f}",
            f"{sa:.0f}%",
            f"{sb:.0f}%",
            f"{ta_:.0f}",
            f"{tb_:.0f}",
            f"{d_tokens:+.0f}",
        )

    console.print(table)

    # Global delta
    g_ca = da["tool_call_count"].mean()
    g_cb = db["tool_call_count"].mean()
    g_sa = (da["tool_call_count"] == 1).mean() * 100
    g_sb = (db["tool_call_count"] == 1).mean() * 100
    g_ta = da["total_tokens"].mean()
    g_tb = db["total_tokens"].mean()
    console.print(
        f"\n[bold]Overall[/bold]:  "
        f"calls {g_ca:.2f} → {g_cb:.2f} (Δ {g_cb - g_ca:+.2f})  |  "
        f"single-shot {g_sa:.0f}% → {g_sb:.0f}% (Δ {g_sb - g_sa:+.0f}pp)  |  "
        f"tokens {g_ta:.0f} → {g_tb:.0f} (Δ {g_tb - g_ta:+.0f})"
    )


if __name__ == "__main__":
    app()
