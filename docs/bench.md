# Запуск aggregation bench

`bench/` — опциональный Python harness, который измеряет, насколько агрессивно LLM
батчит вызовы `query_graphql` при разных описаниях инструмента. Ему **не** нужен
запущенный шлюз vmcp — `mock_tool.py` синтезирует правдоподобный JSON inline, поэтому
метрика отражает поведение модели при batching, а не MCP plumbing.

Требуется:

- Python ≥ 3.11
- [`uv`](https://docs.astral.sh/uv/) (рекомендуется) или эквивалентный venv
- OpenAI-compatible chat API key

## Установка

```bash
cd bench
uv sync
```

## Учётные данные

Задайте API key перед любым настоящим LLM-запуском. **Жёстко заданного fallback нет.**

| Переменная | Обязательна | По умолчанию | Заметки |
| -------- | -------- | ------- | ----- |
| `OPENAI_API_KEY` | yes* | — | Предпочтительно. Alias: `LITELLM_API_KEY`. |
| `OPENAI_BASE_URL` | no | `https://api.openai.com/v1` | Любой OpenAI-compatible base URL. Alias: `LITELLM_BASE_URL`. |
| `OPENAI_MODEL` | no | `gpt-4o-mini` | Можно переопределить через `--model`. |

\* Требуется одна из `OPENAI_API_KEY` / `LITELLM_API_KEY`.

```bash
export OPENAI_API_KEY=sk-...
# optional:
export OPENAI_BASE_URL=https://api.openai.com/v1
export OPENAI_MODEL=gpt-4o-mini
```

## Пробный запуск

Одна задача × одна реплика, низкая concurrency:

```bash
cd bench
uv run python run.py \
  --description descriptions/A_current.txt \
  --runs 1 --concurrency 1 \
  --tag smoke --out results/smoke.jsonl
```

## Полное A/B-сравнение (описания инструментов)

Оставьте system prompt фиксированным (по умолчанию) и меняйте описания инструмента:

```bash
uv run python run.py \
  --description descriptions/A_current.txt \
  --runs 20 --tag A --out results/A.jsonl

uv run python run.py \
  --description descriptions/C_noguidance.txt \
  --runs 20 --tag C --out results/C.jsonl

uv run python analyze.py results/A.jsonl results/C.jsonl
```

`analyze.py` печатает deltas по задачам и общее изменение single-shot rate
(`tool_call_count == 1`).

## A/B-сравнение (системные промпты агента)

Оставьте описание инструмента фиксированным и меняйте системные промпты в стиле harness.
Поставляемые адаптации (см. [`bench/prompts/SOURCES.md`](../bench/prompts/SOURCES.md)):

| Tag | `--system` | Стиль |
| --- | ---------- | ----- |
| `sys_default` | `prompts/system_default.txt` | Минимальная bench baseline |
| `sys_hermes` | `prompts/system_hermes.txt` | Nous Research Hermes Agent |
| `sys_cursor` | `prompts/system_cursor.txt` | Cursor Agent |
| `sys_claude` | `prompts/system_claude_code.txt` | Claude Code harness |

Пример матрицы против описания RULE #1:

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

Также можно скрещивать description × system (например, `C_noguidance` + Hermes) —
ясно помечайте outputs, чтобы строки `analyze.py` оставались сопоставимыми.

## Полезные flags

| Flag | По умолчанию | Значение |
| ---- | ------- | ------- |
| `--description` / `-d` | required | Текстовый файл описания инструмента |
| `--system` / `-s` | `prompts/system_default.txt` | System prompt |
| `--tasks` / `-t` | `tasks/tasks.jsonl` | JSONL с `{id, user_msg}` |
| `--runs` / `-n` | `20` | Replicas per task |
| `--concurrency` / `-c` | `20` | Параллельные запуски |
| `--turn-cap` | `8` | Максимум tool-use rounds на run |
| `--model` / `-m` | `OPENAI_MODEL` or `gpt-4o-mini` | Chat model id |
| `--temperature` | `0.7` | Sampling temperature (`0` для deterministic) |
| `--base-url` | env / OpenAI | OpenAI-compatible base URL |
| `--tag` | `run` | Метка, записываемая в каждую строку JSONL |
| `--out` / `-o` | `results/run.jsonl` | Output path |

## Синхронизация описания из Rust source

После редактирования описания инструмента `query_graphql` в
`crates/vmcp-server/src/lib.rs`:

```bash
cd bench
uv run python _extract_desc.py
```

Записывает live extract в `descriptions/HEAD.txt`.

## Структура

См. [`bench/README.md`](../bench/README.md) для карты каталогов, определения метрики
и известных ограничений. Исторические числа A vs C (1400 runs) лежат в
[`bench/RESULTS.md`](../bench/RESULTS.md).

## Примечания

- Результаты в `bench/results/` gitignored, кроме канонических
  `A_current.jsonl` / `C_noguidance.jsonl`, на которых основан `RESULTS.md`.
- Mock возвращает фиксированные demo fixtures (fake employees/customers). Он не
  подключён к живой базе данных.
- Providers могут вводить rate-limit при высокой concurrency; harness повторяет
  `RateLimitError`, timeouts и connection errors с exponential backoff.
