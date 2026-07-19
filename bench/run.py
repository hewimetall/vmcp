"""Aggregation benchmark harness.

Drives an OpenAI-compatible LLM with one swappable tool description + system
prompt against a set of multi-fact tasks. Measures how many `query_graphql`
calls the model makes per task and how many tokens each run burns. Lower call
count = better aggregation (the LLM batched its sub-questions into one aliased
GraphQL document).

Requires credentials via environment (no hardcoded secrets):

  export OPENAI_API_KEY=...          # or LITELLM_API_KEY
  export OPENAI_BASE_URL=...         # optional; or LITELLM_BASE_URL
  # default base URL: https://api.openai.com/v1

Usage (smoke):
  uv run python run.py --runs 1 --concurrency 1 \
    --tasks tasks/tasks.jsonl \
    --description descriptions/A_current.txt \
    --tag A_smoke --out results/A_smoke.jsonl

Full A/B:
  uv run python run.py --description descriptions/A_current.txt   --tag A --out results/A.jsonl
  uv run python run.py --description descriptions/C_noguidance.txt --tag C --out results/C.jsonl
  uv run python analyze.py results/A.jsonl results/C.jsonl
"""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys
import time
import uuid
from typing import Any

import typer
from openai import (
    APIConnectionError,
    APIError,
    APITimeoutError,
    AsyncOpenAI,
    InternalServerError,
    RateLimitError,
)
from rich.console import Console
from rich.table import Table
from tenacity import (
    AsyncRetrying,
    retry_if_exception_type,
    stop_after_attempt,
    wait_exponential,
)

from mock_tool import extract_top_level_aliases, respond as mock_respond

# ---------- alias-counter unit checks ----------

# Hardcoded sanity: 1 / 3 / 5 alias cases. If these ever fail the metric is
# unreliable — fix the regex in mock_tool.extract_top_level_aliases.
assert len(extract_top_level_aliases('{ servers { name } }')) == 1
assert len(extract_top_level_aliases(
    '{ moscow: time { x } tokyo: time { x } customers: postgres { x } }'
)) == 3
assert len(extract_top_level_aliases(
    '{ a: time { x } b: time { x } c: postgres { x } d: postgres { x } e: postgres { x } }'
)) == 5


# ---------- config ----------

DEFAULT_BASE_URL = "https://api.openai.com/v1"


def _resolve_api_key() -> str | None:
    """Prefer OPENAI_API_KEY; accept LITELLM_API_KEY as an alias."""
    return os.environ.get("OPENAI_API_KEY") or os.environ.get("LITELLM_API_KEY")


def _resolve_base_url() -> str:
    """Prefer OPENAI_BASE_URL; accept LITELLM_BASE_URL as an alias."""
    return (
        os.environ.get("OPENAI_BASE_URL")
        or os.environ.get("LITELLM_BASE_URL")
        or DEFAULT_BASE_URL
    )


def _tool_spec(description: str) -> dict[str, Any]:
    return {
        "type": "function",
        "function": {
            "name": "query_graphql",
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "GraphQL document to execute against the vmcp gateway.",
                    },
                    "variables": {
                        "type": "object",
                        "description": "Optional GraphQL variables.",
                    },
                    "operation_name": {
                        "type": "string",
                        "description": "Optional GraphQL operation name.",
                    },
                },
                "required": ["query"],
            },
        },
    }


# ---------- core run loop ----------


async def _chat(client: AsyncOpenAI, **kwargs: Any) -> Any:
    """Retry wrapper around chat.completions.create."""
    async for attempt in AsyncRetrying(
        stop=stop_after_attempt(5),
        wait=wait_exponential(multiplier=2, min=1, max=30),
        retry=retry_if_exception_type(
            (RateLimitError, APITimeoutError, APIConnectionError, InternalServerError)
        ),
        reraise=True,
    ):
        with attempt:
            return await client.chat.completions.create(**kwargs)
    raise RuntimeError("unreachable")  # pragma: no cover


async def run_once(
    client: AsyncOpenAI,
    *,
    tag: str,
    task: dict[str, Any],
    system: str,
    tool_spec: dict[str, Any],
    model: str,
    temperature: float,
    turn_cap: int,
) -> dict[str, Any]:
    run_id = uuid.uuid4().hex[:12]
    start = time.monotonic()

    messages: list[dict[str, Any]] = [
        {"role": "system", "content": system},
        {"role": "user", "content": task["user_msg"]},
    ]
    tool_calls_log: list[dict[str, Any]] = []
    tool_call_count = 0
    total_aliases = 0
    prompt_tokens = 0
    completion_tokens = 0
    turns = 0
    truncated = False
    final_text = ""
    error_str: str | None = None

    try:
        for turn in range(turn_cap):
            turns = turn + 1
            resp = await _chat(
                client,
                model=model,
                messages=messages,
                tools=[tool_spec],
                tool_choice="auto",
                temperature=temperature,
                max_tokens=2000,
            )
            usage = getattr(resp, "usage", None)
            if usage:
                prompt_tokens += getattr(usage, "prompt_tokens", 0) or 0
                completion_tokens += getattr(usage, "completion_tokens", 0) or 0

            msg = resp.choices[0].message
            assistant_entry: dict[str, Any] = {
                "role": "assistant",
                "content": msg.content,
            }
            if msg.tool_calls:
                assistant_entry["tool_calls"] = [
                    {
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        },
                    }
                    for tc in msg.tool_calls
                ]
            messages.append(assistant_entry)

            if not msg.tool_calls:
                final_text = msg.content or ""
                break

            for tc in msg.tool_calls:
                tool_call_count += 1
                raw_args = tc.function.arguments or "{}"
                try:
                    args = json.loads(raw_args)
                except json.JSONDecodeError:
                    args = {"query": raw_args}
                q = args.get("query", "") if isinstance(args, dict) else ""
                aliases = extract_top_level_aliases(q)
                total_aliases += len(aliases)
                tool_calls_log.append(
                    {
                        "turn": turn,
                        "aliases": len(aliases),
                        "query_excerpt": q[:240],
                    }
                )
                tool_result = mock_respond(args)
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": tc.id,
                        "content": tool_result,
                    }
                )

            if len(messages) > 30:
                truncated = True
                break
        else:
            truncated = True
    except Exception as exc:  # pragma: no cover - network errors etc.
        error_str = f"{type(exc).__name__}: {exc}"

    wall_ms = int((time.monotonic() - start) * 1000)
    return {
        "run_id": run_id,
        "tag": tag,
        "task_id": task.get("id", ""),
        "model": model,
        "temperature": temperature,
        "turns": turns,
        "tool_call_count": tool_call_count,
        "total_aliases": total_aliases,
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
        "truncated": truncated,
        "wall_ms": wall_ms,
        "tool_calls": tool_calls_log,
        "final_text_excerpt": final_text[:500],
        "error": error_str,
    }


# ---------- summary ----------


def _percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    s = sorted(values)
    k = (len(s) - 1) * p
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return float(s[f])
    return float(s[f] + (s[c] - s[f]) * (k - f))


def _print_summary(rows: list[dict[str, Any]], console: Console) -> None:
    if not rows:
        console.print("[red]No rows recorded.[/red]")
        return

    by_task: dict[str, list[dict[str, Any]]] = {}
    for r in rows:
        by_task.setdefault(r["task_id"], []).append(r)

    table = Table(title="Per-task summary (lower tool_call_count = better aggregation)")
    table.add_column("task_id", style="cyan", no_wrap=True)
    table.add_column("n_runs", justify="right")
    table.add_column("calls μ", justify="right")
    table.add_column("calls p50", justify="right")
    table.add_column("calls p95", justify="right")
    table.add_column("%single", justify="right", style="green")
    table.add_column("tokens μ", justify="right")
    table.add_column("wall μ (ms)", justify="right")
    table.add_column("errors", justify="right", style="red")

    for task_id, items in by_task.items():
        calls = [r["tool_call_count"] for r in items]
        tokens = [r["total_tokens"] for r in items]
        wall = [r["wall_ms"] for r in items]
        errs = sum(1 for r in items if r.get("error"))
        single_rate = sum(1 for c in calls if c == 1) / len(calls) * 100
        table.add_row(
            task_id,
            str(len(items)),
            f"{sum(calls)/len(calls):.2f}",
            f"{_percentile(calls, 0.5):.1f}",
            f"{_percentile(calls, 0.95):.1f}",
            f"{single_rate:.0f}%",
            f"{sum(tokens)/len(tokens):.0f}",
            f"{sum(wall)/len(wall):.0f}",
            str(errs),
        )

    console.print(table)

    # Global footer
    calls_all = [r["tool_call_count"] for r in rows]
    tokens_all = [r["total_tokens"] for r in rows]
    console.print(
        f"\n[bold]Overall[/bold]: n={len(rows)}  "
        f"mean calls={sum(calls_all)/len(calls_all):.2f}  "
        f"single-shot rate={sum(1 for c in calls_all if c==1)/len(calls_all)*100:.0f}%  "
        f"mean tokens={sum(tokens_all)/len(tokens_all):.0f}"
    )


# ---------- CLI ----------

app = typer.Typer(add_completion=False)


@app.command()
def main(
    description: pathlib.Path = typer.Option(
        ..., "--description", "-d", help="Path to tool description text file."
    ),
    system: pathlib.Path = typer.Option(
        pathlib.Path("prompts/system_default.txt"), "--system", "-s",
        help="Path to system prompt text file."
    ),
    tasks: pathlib.Path = typer.Option(
        pathlib.Path("tasks/tasks.jsonl"), "--tasks", "-t",
        help="Path to JSONL of {id, user_msg} tasks."
    ),
    runs: int = typer.Option(20, "--runs", "-n", help="Runs per task."),
    concurrency: int = typer.Option(20, "--concurrency", "-c"),
    turn_cap: int = typer.Option(8, "--turn-cap", help="Max tool-use rounds per run."),
    model: str = typer.Option(
        os.environ.get("OPENAI_MODEL", "gpt-4o-mini"),
        "--model", "-m",
        help="Chat model id (default: OPENAI_MODEL or gpt-4o-mini).",
    ),
    temperature: float = typer.Option(0.7, "--temperature"),
    tag: str = typer.Option("run", "--tag", help="Label written into every row."),
    out: pathlib.Path = typer.Option(
        pathlib.Path("results/run.jsonl"), "--out", "-o",
        help="Output JSONL path."
    ),
    base_url: str | None = typer.Option(
        None, "--base-url",
        help="OpenAI-compat base URL (default: OPENAI_BASE_URL / LITELLM_BASE_URL / api.openai.com).",
    ),
) -> None:
    """Run the benchmark and write JSONL + print summary."""
    console = Console()

    api_key = _resolve_api_key()
    if not api_key:
        console.print(
            "[red]Set OPENAI_API_KEY (or LITELLM_API_KEY) before running the bench.[/red]"
        )
        raise typer.Exit(2)
    resolved_base_url = base_url or _resolve_base_url()

    desc_text = description.read_text(encoding="utf-8").strip()
    sys_text = system.read_text(encoding="utf-8")
    # Strip only a leading provenance comment block (`# ...` lines / blanks
    # at the top of the file). Do NOT strip later markdown headings — those
    # are part of the prompt body (Hermes / Cursor / Claude Code styles).
    body_lines = sys_text.splitlines()
    i = 0
    while i < len(body_lines) and (
        not body_lines[i].strip() or body_lines[i].lstrip().startswith("#")
    ):
        i += 1
    sys_text = "\n".join(body_lines[i:]).strip()

    task_rows: list[dict[str, Any]] = []
    for line in tasks.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("//"):
            continue
        task_rows.append(json.loads(line))
    if not task_rows:
        console.print(f"[red]No tasks loaded from {tasks}[/red]")
        raise typer.Exit(2)

    console.print(
        f"[bold]Bench config[/bold]: model={model}  base_url={resolved_base_url}  "
        f"tasks={len(task_rows)}  runs/task={runs}  concurrency={concurrency}  "
        f"description={description.name}  tag={tag}"
    )

    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        out.unlink()
    out_fh = out.open("a", encoding="utf-8")

    tool_spec = _tool_spec(desc_text)

    async def driver() -> list[dict[str, Any]]:
        # Explicit per-request timeout — default is 600s which let stuck
        # provider connections hang the whole bench. 90s is generous enough
        # for a multi-tool-call response to stream.
        client = AsyncOpenAI(base_url=resolved_base_url, api_key=api_key, timeout=90.0)
        sem = asyncio.Semaphore(concurrency)
        results: list[dict[str, Any]] = []
        total = len(task_rows) * runs
        done = 0

        async def gated(task: dict[str, Any]) -> dict[str, Any]:
            async with sem:
                return await run_once(
                    client,
                    tag=tag,
                    task=task,
                    system=sys_text,
                    tool_spec=tool_spec,
                    model=model,
                    temperature=temperature,
                    turn_cap=turn_cap,
                )

        coros = [gated(t) for t in task_rows for _ in range(runs)]
        for fut in asyncio.as_completed(coros):
            row = await fut
            results.append(row)
            out_fh.write(json.dumps(row, ensure_ascii=False) + "\n")
            out_fh.flush()
            done += 1
            status = "ok" if row.get("error") is None else "ERR"
            console.print(
                f"  [{done:>3}/{total}] task={row['task_id']:<28} "
                f"calls={row['tool_call_count']} turns={row['turns']} "
                f"tokens={row['total_tokens']} wall={row['wall_ms']}ms [{status}]"
            )
        return results

    try:
        rows = asyncio.run(driver())
    finally:
        out_fh.close()

    console.print(f"\nWrote {out} ({sum(1 for _ in out.open(encoding='utf-8'))} rows)\n")
    _print_summary(rows, console)


if __name__ == "__main__":
    app()
