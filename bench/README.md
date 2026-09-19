# `vmcp/bench/` — `query_graphql` aggregation benchmark

**Language:** English | [Русский](README.ru.md)

Measures how aggressively an LLM batches `query_graphql` calls when given
different tool descriptions. No vmcp instance is required: `mock_tool.py`
synthesizes the tool result inline, returning plausible JSON sufficient to
answer the canonical tasks in ONE batched call.

**Operator guide:** [`docs/bench.md`](../docs/bench.md)

## Metric

For each run, the harness records the number of `query_graphql` tool calls and
the total token count. A lower call count indicates that the LLM is more likely
to combine subquestions into a single document using aliases. The primary
metric is the **single-shot rate**: the percentage of runs where
`tool_call_count == 1`.

## Quick start

```bash
cd bench
uv sync

export OPENAI_API_KEY=sk-...
# optional overrides:
# export OPENAI_BASE_URL=https://api.openai.com/v1
# export OPENAI_MODEL=gpt-4o-mini

# smoke: 1 task × 1 run
uv run python run.py \
  --description descriptions/A_current.txt \
  --runs 1 --concurrency 1 --tag smoke --out results/smoke.jsonl

# full: 7 tasks × 20 runs
uv run python run.py \
  --description descriptions/A_current.txt \
  --runs 20 --tag A --out results/A.jsonl
```

## A/B test tool descriptions

```bash
uv run python run.py --description descriptions/A_current.txt    --runs 20 --tag A --out results/A.jsonl
uv run python run.py --description descriptions/C_noguidance.txt --runs 20 --tag C --out results/C.jsonl
uv run python analyze.py results/A.jsonl results/C.jsonl
```

`analyze.py` prints per-task deltas and the overall change in single-shot rate.

## A/B test system prompts (Hermes / Cursor / Claude Code)

Keep `--description` fixed and vary `--system`:

```bash
DESC=descriptions/A_current.txt
for pair in \
  "sys_default:prompts/system_default.txt" \
  "sys_hermes:prompts/system_hermes.txt" \
  "sys_cursor:prompts/system_cursor.txt" \
  "sys_claude:prompts/system_claude_code.txt"
do
  TAG="${pair%%:*}"; SYS="${pair#*:}"
  uv run python run.py -d "$DESC" -s "$SYS" --runs 20 --tag "$TAG" -o "results/${TAG}.jsonl"
done
uv run python analyze.py results/sys_default.jsonl results/sys_hermes.jsonl
```

Sources and adaptation notes: [`prompts/SOURCES.md`](prompts/SOURCES.md).
Full operator guide: [`docs/bench.md`](../docs/bench.md).

## Layout

```
run.py                 # async harness (Typer CLI)
mock_tool.py           # query → fake JSON dispatcher
analyze.py             # pandas diff of two JSONL outputs
_extract_desc.py       # one-shot: copy lib.rs description → descriptions/HEAD.txt
pyproject.toml         # uv deps: openai, tenacity, typer, rich, pandas

descriptions/
  HEAD.txt             # live extract — always mirrors crates/vmcp-server/src/lib.rs
  A_current.txt        # historical snapshot — backs the 1400-run A/C results in RESULTS.md
  A_v2.txt             # iteration after A: adds RULE #1B + #1C anti-patterns
  B_terse.txt          # 3-paragraph minimal
  C_noguidance.txt     # control: bare schema, no batching hint
prompts/
  system_default.txt       # minimal system prompt
  system_custom.txt        # optional blank alternate
  system_hermes.txt        # Hermes Agent–style (Nous Research)
  system_cursor.txt        # Cursor Agent–style
  system_claude_code.txt   # Claude Code harness–style
  SOURCES.md               # provenance for the adaptations above
tasks/
  tasks.jsonl          # 7 multi-fact tasks; first row = canonical demo summary
results/               # gitignored — JSONL outputs land here
```

## Update the description after editing lib.rs

```bash
uv run python _extract_desc.py
```

This reads `crates/vmcp-server/src/lib.rs`, locates the
`#[tool(description = ...)]` block (supporting both raw strings such as
`r#"..."#` and escaped `"..."` strings with Rust line continuations), and
writes it to `descriptions/HEAD.txt`. Run it after every change to the tool
description so the benchmark's `HEAD.txt` remains synchronized with the shipped
source.

## How alias counting works

The top-level alias regex `(\w+)\s*:\s*(\w+)\s*\{` counts each
`<alias>: <server> {` within the query. This signal is sufficient and does not
depend on `graphql-core`. Startup sanity assertions in `run.py` check three
known queries containing 1, 3, and 5 aliases.

## Limitations

- **Concurrency**: the default is 20; providers may impose rate limits at
  higher values. `tenacity` retries `RateLimitError`, `APITimeoutError`, and
  connection errors with exponential backoff.
- **Determinism**: the default is `temperature=0.7`, so 20 runs of the same task
  will diverge. Pass `--temperature 0` for variance-free baselines.
- **Truncation**: each run is limited to `--turn-cap 8` rounds and 30 messages.
  Rows are marked `"truncated": true`. Increase the cap if the LLM genuinely
  needs more turns.
- **Mock fidelity**: row counts and salaries are static fixtures. The benchmark
  measures LLM batching decisions, not data correctness.
