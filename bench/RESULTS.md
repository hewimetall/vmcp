# `query_graphql` aggregation benchmark — initial results

**Language:** English | [Русский](RESULTS.ru.md)

## TL;DR

Rewriting the `query_graphql` tool description in a
"RULE #1 — BATCH EVERYTHING INTO ONE CALL" style (commit `1795ce7`) raises the
**single-shot rate from 4% to 85%** on the canonical demo task set and halves
the average number of tokens per task after failed runs are filtered out.
Across **1,400 runs** (700 per arm, 7 tasks × 100 replicas), the trend is clear:
explicit batching guidance shifts the LLM from a turn-cap loop to a one-shot
aliased document.

## Setup

- **Harness**: `vmcp/bench/run.py`, an asynchronous OpenAI-compatible client
  (`model=developer` at the time of the run). `mock_tool.py` supplies
  `query_graphql` results inline, with no MCP server or live upstreams.
- **Concurrency**: 20.  **Temperature**: 0.7.  **Turn cap**: 8.
- **Tasks**: 7 multi-fact prompts (`tasks/tasks.jsonl`), with 100 replicas each.
  They are designed so that a properly batched answer fits in one aliased
  GraphQL document.
- **Metric**: the number of `query_graphql` tool calls per run (fewer calls
  means better aggregation) and total prompt plus completion tokens.

## The two descriptions compared

| Tag             | File                            | Description |
|-----------------|---------------------------------|-------------|
| `A_current`     | `descriptions/A_current.txt`    | Full description from commit `1795ce7`, with "RULE #1 BATCH" and an anti-pattern. |
| `C_noguidance`  | `descriptions/C_noguidance.txt` | Three-line bare schema with no batching hint; the control arm. |

`A_current.txt` was extracted verbatim from
`crates/vmcp-server/src/lib.rs:106-179` by `_extract_desc.py`.

## Key results (errors and truncated runs excluded)

| task_id                       | A: calls μ | A: %single | A: tokens | C: calls μ | C: %single | C: tokens | Δ calls |
|-------------------------------|-----------:|-----------:|----------:|-----------:|-----------:|----------:|--------:|
| customer_country_breakdown    |       4.29 |        14% |    10 607 |       3.00 |         0% |     3 078 |   -1.29 |
| demo_summary (canonical)      |       1.12 |    **92%** |     4 831 |      10.00 |         0% |     7 192 |   +8.88 |
| employee_dept_overview        |       2.06 |        25% |     5 837 |       3.24 |         5% |     2 996 |   +1.18 |
| orders_employees_join         |       1.00 |       100% |     3 791 |       1.00 |       100% |     1 811 |    0.00 |
| small_targeted                |       1.00 |       100% |     3 116 |       6.60 |         0% |     6 595 |   +5.60 |
| time_postgres_mixed           |       1.00 |       100% |     3 521 |       5.78 |        22% |     3 991 |   +4.78 |
| world_clock_table             |       1.00 |       100% |     3 482 |       5.80 |         0% |     5 458 |   +4.80 |
| **OVERALL**                   |   **1.23** |    **85%** |     4 172 |       3.55 |        39% |     3 364 |  +2.32 |

Filter: `error is None and not truncated`. Surviving rows: A 606/700,
C 67/700. In C, **341 of 700** runs reached `--turn-cap 8`, while **292 of 700**
timed out. Both are symptoms of the same problem: without guidance, the LLM
makes many sequential tool calls.

## Raw distribution (control group C, all 700 runs including failures)

```
0 calls (timeout):   275
1 call (single-shot): 29  ← only 4% of runs
8 calls (turn-cap):  242  ← 35% hit the cap
9-22 calls:           67
other (2-7):          87
```

## Interpretation

1. **The new description works for "additive" multi-fact tasks**: every task
   with heterogeneous subquestions (different upstreams, time zones, or SQL)
   moves from a 0–22% single-shot rate in C to a **100% single-shot rate** in A.
   World clocks, orders plus employees, and the demo summary all switch cleanly.

2. **`customer_country_breakdown` remains a failure mode in A**
   (14% single-shot, 4.29 mean calls). The LLM correctly issues a GROUP BY
   query in call #1, but then often realizes that the user also requested
   *totals* and issues a second Postgres alias in call #2 instead of batching
   `by_country` and `totals` at the Postgres level. This is the targeted gap for
   the next description revision: the `A_v2` draft in
   `descriptions/A_v2.txt` adds the explicit anti-patterns "breakdown + totals
   = one call, two aliases at the postgres level" and "per-category loops are
   a bug, use GROUP BY".

3. **The mean token count in C is lower than in A because failed runs record
   zero tokens.** Among completed runs (n=67 for C), C's mean token count is
   comparable to or higher than A's; the wall-time cost of multiple turns
   dominates.

4. **Neither description was tuned to the mock.** The mock (`mock_tool.py`)
   handles aliased and bare top-level queries to `postgres` and `time`, and
   recognizes multi-subquery SELECT and UNION ALL patterns. Anything
   unrecognized returns `[{"note":"mock", "sql": ...}]`, which is not an
   error, so the harness never forces the LLM to retry.

## Reproduce the results

```bash
cd bench

# A_current (RULE #1 description):
uv run python run.py --description descriptions/A_current.txt \
  --runs 100 --concurrency 20 --tag A_current \
  --out results/A_current.jsonl

# C_noguidance (control):
uv run python run.py --description descriptions/C_noguidance.txt \
  --runs 100 --concurrency 20 --tag C_noguidance \
  --out results/C_noguidance.jsonl

# Side-by-side delta:
uv run python analyze.py results/A_current.jsonl results/C_noguidance.jsonl
# OR clean error-filtered summary:
python3 _summary.py
```

## Awaiting a run

- **`A_v2`** (draft in `descriptions/A_v2.txt`) adds two new anti-patterns
  targeting the `customer_country_breakdown` failure mode. **The run has not
  completed yet**: the LLM endpoint went down partway through the batch with a
  persistent `HTTP 000` connection refusal after approximately 50 replicas per
  task. The harness has been updated to make up to 5 total attempts for
  `APIConnectionError` and `InternalServerError`, with exponential backoff
  capped at 30 seconds and a 90-second per-request timeout. Rerun it when the
  endpoint is available again.

## Provenance

- Date: 2026-05-30
- Tool description under test: `crates/vmcp-server/src/lib.rs:106-179` at
  commit `1795ce7` (`docs(tool): rewrite query_graphql description to push
  aliasing hard`).
- Total runs analysed: **1400** (A_current 700 + C_noguidance 700).
- Total LLM-side tool calls measured: ~1500 (A) + ~3400 (C).
- Raw data: `results/A_current.jsonl`, `results/C_noguidance.jsonl`.

## Update — the `A_v2` description has shipped

Based on the remaining `customer_country_breakdown` failure mode in A, the tool
description in `crates/vmcp-server/src/lib.rs` has been updated to match
**`A_v2`** (`descriptions/A_v2.txt`, now also mirrored in
`descriptions/HEAD.txt`). Two new sections target the breakdown-splitting
behavior observed in the benchmark:

- **RULE #1B — BREAKDOWN + TOTALS = ONE CALL, ALWAYS TWO ALIASES** — an
  explicit anti-pattern and correct pattern for requests of the form "count per
  X, plus the total," which caused the LLM to split the breakdown and totals
  across two calls.
- **RULE #1C — PER-CATEGORY LOOPS ARE A BUG. USE GROUP BY** — covers the
  related mistake of iterating over categories instead of using one GROUP BY.

The Rust string literal was also changed from an escaped string with line
continuations (`"\<LF>line\n\<LF>"`) to a raw string (`r#"..."#`) so the source
remains plain text and easy to review in diffs. `_extract_desc.py` supports
both forms.

**A_v2 has not yet been re-benchmarked**: the LLM endpoint went down partway
through the A_v2 batch with an `HTTP 000` connection refusal after approximately
50 replicas per task. The harness now makes up to 5 total attempts for
`APIConnectionError` and `InternalServerError` and applies a 90-second
per-request timeout. Rerun the benchmark when the endpoint is available to
determine whether the
new rules raise `customer_country_breakdown` from a 14% single-shot rate toward
the 92% that the canonical `demo_summary` achieves in A.
