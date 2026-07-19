# `vmcp/bench/` — benchmark агрегации `query_graphql`

Измеряет, насколько агрессивно LLM батчит вызовы `query_graphql`, когда получает
разные описания инструмента. Экземпляр vmcp не нужен — результат инструмента
синтезируется inline в `mock_tool.py` (возвращает правдоподобный JSON, достаточный,
чтобы ответить на канонические задачи ОДНИМ пакетным вызовом).

**Операторское руководство:** [`docs/bench.md`](../docs/bench.md)

## Метрика

На каждый run: число вызовов инструмента `query_graphql` + total tokens. Чем меньше
call count, тем вероятнее LLM объединяет под-вопросы в один документ через aliases. Главное число —
**single-shot rate**: % runs, где `tool_call_count == 1`.

## Быстрый старт

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

## A/B-сравнение описаний

```bash
uv run python run.py --description descriptions/A_current.txt    --runs 20 --tag A --out results/A.jsonl
uv run python run.py --description descriptions/C_noguidance.txt --runs 20 --tag C --out results/C.jsonl
uv run python analyze.py results/A.jsonl results/C.jsonl
```

`analyze.py` печатает delta по задачам + общее изменение single-shot rate.

## A/B-сравнение системных промптов (Hermes / Cursor / Claude Code)

Оставьте `--description` фиксированным и меняйте `--system`:

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

Источники и заметки об адаптации: [`prompts/SOURCES.md`](prompts/SOURCES.md).
Полное операторское руководство: [`docs/bench.md`](../docs/bench.md).

## Структура

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

## Обновить описание после редактирования lib.rs

```bash
uv run python _extract_desc.py
```

Читает `crates/vmcp-server/src/lib.rs`, находит блок `#[tool(description = ...)]`
(поддерживает raw strings `r#"..."#` и escaped `"..."` с Rust line-continuations),
записывает в `descriptions/HEAD.txt`. Запускайте после каждого изменения описания
инструмента, чтобы bench-time HEAD.txt был синхронизирован с shipped source.

## Как работает счётчик aliases

Regex для top-level alias `(\w+)\s*:\s*(\w+)\s*\{` считает каждый `<alias>: <server> {`
внутри query. Сигнал достаточный; он не зависит от `graphql-core`. Sanity asserts в начале
`run.py` проверяют три известных query (1, 3, 5 aliases).

## Ограничения

- **Concurrency**: 20 — значение по умолчанию; providers могут вводить rate-limit, если выше.
  `tenacity` делает retry с exp backoff при `RateLimitError` / `APITimeoutError`
  / connection errors.
- **Determinism**: `temperature=0.7` по умолчанию, поэтому 20 runs одной задачи расходятся.
  Для variance-free baselines передайте `--temperature 0`.
- **Truncation**: каждый run ограничен `--turn-cap 8` rounds и 30 messages.
  Строки помечаются как `"truncated": true`. Увеличьте cap, если LLM действительно нужно
  больше turns.
- **Mock fidelity**: row counts и salaries — статические fixtures. Мы измеряем
  LLM batching decisions, а не корректность данных.
