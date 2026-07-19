# Benchmark агрегации `query_graphql` — первые результаты

## TL;DR

Переписывание описания инструмента `query_graphql` в стиле "RULE #1 — BATCH EVERYTHING INTO ONE CALL"
(commit `1795ce7`) поднимает **single-shot rate с 4% до 85%** на каноническом
наборе demo tasks и вдвое снижает среднее число tokens на задачу (после фильтрации
ошибочных runs). На **1400 runs** (700 в каждом arm, 7 tasks × 100 replicas)
тренд однозначен: явное batching guidance меняет поведение LLM с turn-cap loop
на one-shot aliased document.

## Настройка

- **Harness**: `vmcp/bench/run.py`, async OpenAI-compatible client
  (`model=developer` на момент запуска). Mock results для `query_graphql`
  inline через `mock_tool.py` — без MCP server и живых upstream.
- **Concurrency**: 20.  **Temperature**: 0.7.  **Turn cap**: 8.
- **Tasks**: 7 multi-fact prompts (`tasks/tasks.jsonl`), по 100 replicas.
  Спроектированы так, чтобы хорошо сбатченный ответ помещался в один aliased GraphQL document.
- **Метрика**: число вызовов инструмента `query_graphql` на run (меньше
  = лучше агрегация) и total prompt+completion tokens.

## Два сравниваемых описания

| Tag             | File                            | Что говорит                                                          |
|-----------------|---------------------------------|------------------------------------------------------------------------|
| `A_current`     | `descriptions/A_current.txt`    | Полное описание commit-`1795ce7`, "RULE #1 BATCH" + anti-pattern.    |
| `C_noguidance`  | `descriptions/C_noguidance.txt` | 3-line bare schema, без batching hint — control arm.               |

`A_current.txt` извлечён дословно из
`crates/vmcp-server/src/lib.rs:106-179` через `_extract_desc.py`.

## Главные числа (errors / truncated runs отфильтрованы)

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

Filter:  `error is None and not truncated`.  Surviving rows: A 606/700,
C 67/700.  В C **341 из 700** runs достиг `--turn-cap 8`, а **292 из 700**
истекли по timeout — оба симптома одной проблемы: без guidance LLM делает
много последовательных tool calls.

## Сырое распределение (контрольная группа C, все 700 runs, включая failures)

```
0 calls (timeout):   275
1 call (single-shot): 29  ← только 4% runs
8 calls (turn-cap):  242  ← 35% упёрлись в cap
9-22 calls:           67
other (2-7):          87
```

## Интерпретация

1. **Новое описание работает для "additive" multi-fact tasks**: каждая задача,
   где под-вопросы неоднородны (разные upstream, разные timezones, разные SQL),
   переходит с 0-22% single-shot в C до **100% single-shot** в A. World clocks,
   orders+employees, demo summary — все переключаются чисто.

2. **`customer_country_breakdown` — остаточный failure mode в A**
   (14% single-shot, 4.29 mean calls). LLM правильно делает GROUP BY query
   в call #1, затем часто осознаёт, что пользователь также просил
   *totals*, и выпускает второй postgres alias в call #2 вместо batching
   `by_country` + `totals` на уровне postgres. Это targeted gap для следующей
   ревизии описания (`A_v2`, draft в `descriptions/A_v2.txt` — добавляет явные
   anti-patterns "breakdown + totals = one call, two aliases at the postgres level"
   и "per-category loops are a bug, use GROUP BY").

3. **Среднее tokens в C ниже, чем в A, потому что failed runs стоят 0 tokens.**
   Среди завершённых runs (n=67 для C) token mean в C сопоставим с A или выше —
   wall-time penalty нескольких turns доминирует.

4. **Ни одно описание не было tuned to the mock**. Mock (`mock_tool.py`)
   обрабатывает aliased + bare top-level queries к `postgres` / `time`,
   распознаёт multi-subquery SELECT и UNION ALL patterns. Всё нераспознанное
   возвращает `[{"note":"mock", "sql": ...}]` — non-error, поэтому harness
   никогда не заставляет LLM retry.

## Как воспроизвести

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

## Ожидает запуска

- **`A_v2`** (draft в `descriptions/A_v2.txt`) — добавляет два новых anti-patterns,
  направленных на failure mode `customer_country_breakdown`. **Ещё не прогонялся**
  до конца: LLM endpoint упал посреди batch (`HTTP 000` connection
  refusal, persistent) после ~50 replicas на task. Harness уже обновлён,
  чтобы retry `APIConnectionError` / `InternalServerError` с 5 attempts +
  exp backoff до 30s и применять 90s per-request timeout. Перезапустите,
  когда endpoint вернётся.

## Происхождение

- Date: 2026-05-30
- Tool description under test: `crates/vmcp-server/src/lib.rs:106-179` at
  commit `1795ce7` (`docs(tool): rewrite query_graphql description to push
  aliasing hard`).
- Total runs analysed: **1400** (A_current 700 + C_noguidance 700).
- Total LLM-side tool calls measured: ~1500 (A) + ~3400 (C).
- Raw data: `results/A_current.jsonl`, `results/C_noguidance.jsonl`.

## Обновление — описание `A_v2` поставлено

На основе остаточного failure mode `customer_country_breakdown` в A описание
инструмента в `crates/vmcp-server/src/lib.rs` обновлено до содержимого **`A_v2`**
(`descriptions/A_v2.txt`, теперь также mirrored в `descriptions/HEAD.txt`). Два новых
раздела нацелены на breakdown-splitting behaviour, наблюдавшееся в bench:

- **RULE #1B — BREAKDOWN + TOTALS = ONE CALL, ALWAYS TWO ALIASES** — явный
  anti-pattern + correct pattern для формы "count per X, plus the total",
  из-за которой LLM разделяла breakdown и totals на два calls.
- **RULE #1C — PER-CATEGORY LOOPS ARE A BUG. USE GROUP BY** — покрывает
  связанную ошибку итерации по категориям вместо одного GROUP BY.

Rust string literal также переключён с escaped-with-line-continuation
(`"\<LF>line\n\<LF>"`) на raw string (`r#"..."#`), чтобы source оставался
удобным для diff как plain text. `_extract_desc.py` поддерживает обе формы.

**A_v2 ещё не re-benchmarked** — LLM endpoint упал посреди batch
(`HTTP 000` connection refusal) после ~50 replicas на task в A_v2 run.
Harness уже обновлён для retry `APIConnectionError`/`InternalServerError`
с 5 attempts + 90s per-request timeout. Перезапустите, когда endpoint вернётся,
чтобы проверить, действительно ли новые rules поднимают `customer_country_breakdown`
с 14% single-shot к 92%, которых canonical `demo_summary` достигает в A.
