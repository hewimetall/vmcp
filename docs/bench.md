# Running the aggregation benchmark

**Language:** English | [Русский](ru/bench.md)

`bench/` is an optional Python harness that measures how aggressively an LLM
batches `query_graphql` calls under different tool descriptions. It does **not**
require a running vmcp gateway: `mock_tool.py` synthesizes realistic JSON inline,
so the metric reflects the model's batching behavior rather than MCP plumbing.

Requirements:

- Python ≥ 3.11
- [`uv`](https://docs.astral.sh/uv/) (recommended) or an equivalent virtual environment
- An OpenAI-compatible chat API key

## Setup

```bash
cd bench
uv sync
```

## Credentials

Set an API key before making any real LLM request. **There is no hard-coded fallback.**

| Variable | Required | Default | Notes |
| -------- | -------- | ------- | ----- |
| `OPENAI_API_KEY` | yes* | — | Preferred. Alias: `LITELLM_API_KEY`. |
| `OPENAI_BASE_URL` | no | `https://api.openai.com/v1` | Any OpenAI-compatible base URL. Alias: `LITELLM_BASE_URL`. |
| `OPENAI_MODEL` | no | `gpt-4o-mini` | Can be overridden with `--model`. |

\* One of `OPENAI_API_KEY` / `LITELLM_API_KEY` is required.

```bash
export OPENAI_API_KEY=sk-...
# optional:
export OPENAI_BASE_URL=https://api.openai.com/v1
export OPENAI_MODEL=gpt-4o-mini
```

## Smoke run

One task × one replica at low concurrency:

```bash
cd bench
uv run python run.py \
  --description descriptions/A_current.txt \
  --runs 1 --concurrency 1 \
  --tag smoke --out results/smoke.jsonl
```

## Full A/B comparison (tool descriptions)

Keep the system prompt fixed (the default) and vary the tool description:

```bash
uv run python run.py \
  --description descriptions/A_current.txt \
  --runs 20 --tag A --out results/A.jsonl

uv run python run.py \
  --description descriptions/C_noguidance.txt \
  --runs 20 --tag C --out results/C.jsonl

uv run python analyze.py results/A.jsonl results/C.jsonl
```

`analyze.py` prints per-task deltas and the overall change in the single-shot
rate (`tool_call_count == 1`).

## A/B comparison (agent system prompts)

Keep the tool description fixed and vary the harness-style system prompts.
Bundled adaptations (see [`bench/prompts/SOURCES.md`](../bench/prompts/SOURCES.md)):

| Tag | `--system` | Style |
| --- | ---------- | ----- |
| `sys_default` | `prompts/system_default.txt` | Minimal benchmark baseline |
| `sys_hermes` | `prompts/system_hermes.txt` | Nous Research Hermes Agent |
| `sys_cursor` | `prompts/system_cursor.txt` | Cursor Agent |
| `sys_claude` | `prompts/system_claude_code.txt` | Claude Code harness |

Example matrix using the RULE #1 description:

```bash
DESC=descriptions/A_current.txt
RUNS=20

for pair in \
  "sys_default:prompts/system_default.txt" \
  "sys_hermes:prompts/system_hermes.txt" \
  "sys_cursor:prompts/system_cursor.txt" \
  "sys_claude:prompts/system_claude_code.txt"
do
  TAG="${pair%%:*}"
  SYS="${pair#*:}"
  uv run python run.py \
    --description "$DESC" \
    --system "$SYS" \
    --runs "$RUNS" --tag "$TAG" \
    --out "results/${TAG}.jsonl"
done

# pairwise deltas vs default:
uv run python analyze.py results/sys_default.jsonl results/sys_hermes.jsonl
uv run python analyze.py results/sys_default.jsonl results/sys_cursor.jsonl
uv run python analyze.py results/sys_default.jsonl results/sys_claude.jsonl
```

You can also combine description × system variants (for example,
`C_noguidance` + Hermes). Label outputs clearly so the `analyze.py` rows remain
comparable.

## Useful options

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--description` / `-d` | required | Text file containing the tool description |
| `--system` / `-s` | `prompts/system_default.txt` | System prompt |
| `--tasks` / `-t` | `tasks/tasks.jsonl` | JSONL containing `{id, user_msg}` |
| `--runs` / `-n` | `20` | Replicas per task |
| `--concurrency` / `-c` | `20` | Concurrent runs |
| `--turn-cap` | `8` | Maximum tool-use rounds per run |
| `--model` / `-m` | `OPENAI_MODEL` or `gpt-4o-mini` | Chat model id |
| `--temperature` | `0.7` | Sampling temperature (`0` for deterministic output) |
| `--base-url` | env / OpenAI | OpenAI-compatible base URL |
| `--tag` | `run` | Label written to every JSONL row |
| `--out` / `-o` | `results/run.jsonl` | Output path |

## Synchronizing the description from the Rust source

After editing the `query_graphql` tool description in
`crates/vmcp-server/src/lib.rs`, run:

```bash
cd bench
uv run python _extract_desc.py
```

This writes the current extracted description to `descriptions/HEAD.txt`.

## Layout

See [`bench/README.md`](../bench/README.md) for the directory map, metric
definition, and known limitations. Historical A vs C results from 1,400 runs
are in [`bench/RESULTS.md`](../bench/RESULTS.md).

## Notes

- Results in `bench/results/` are gitignored except for the canonical
  `A_current.jsonl` / `C_noguidance.jsonl` files on which `RESULTS.md` is based.
- The mock returns fixed demo fixtures (fake employees/customers). It is not
  connected to a live database.
- Providers may apply rate limits at high concurrency. The harness retries
  `RateLimitError`, timeouts, and connection errors with exponential backoff.
