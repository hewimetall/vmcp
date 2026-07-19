# Нативные MCP Tasks (`run_task`)

vmcp может открывать **SEP-1686** (ревизия спецификации 2025-11-25) для долгих
upstream-инструментов, не превращая каждый GraphQL-вызов в задачу.

| Путь | Опыт клиента | Кто ждёт |
| ---- | ----------------- | --------- |
| `query_graphql` | Немедленный GraphQL JSON | **Шлюз** ждёт upstream (sync) |
| `run_task` без `task` | Немедленный `CallToolResult` | **Шлюз** ждёт один upstream-инструмент |
| `run_task` с `task: {}` | Немедленный `CreateTaskResult` | **Клиент** опрашивает `tasks/get` / `tasks/result` |

Устойчивые строки задач живут во **встроенном SQLite** (WAL).

---

## Включение

По умолчанию выключено. В `vmcp.toml`:

```toml
[tasks]
enabled = true
db_path = "state/tasks.db"   # created automatically (parent dirs too)
task_ttl_ms = 300000         # retention advertised on CreateTaskResult
poll_interval_ms = 2000      # hint for clients
max_concurrent = 16          # in-flight upstream proxies
```

Env overrides (figment):

```bash
VMCP_TASKS__ENABLED=true
VMCP_TASKS__DB_PATH=/var/lib/vmcp/tasks.db
VMCP_TASKS__MAX_CONCURRENT=8
```

При запуске, если `enabled`, но **ни один** upstream-инструмент не поддерживает задачи,
vmcp логирует предупреждение и **не** регистрирует capability `run_task` / `tasks`.

---

## Какие инструменты появляются в `run_task`

В allowlist попадают только инструменты, помеченные как task-capable:

1. Upstream `tools/list` → `execution.taskSupport` = `optional` | `required`
2. Sidecar override в `spec_dir/<server>.json`:

```json
{
  "server": "presentation",
  "tools": [
    { "name": "build_presentation", "read_only": false, "task_support": "optional" },
    { "name": "deploy_presentation", "read_only": false, "task_support": "optional" }
  ]
}
```

| `task_support` | Значение |
| -------------- | ------- |
| omitted / `forbidden` | Только GraphQL (не в `run_task`) |
| `optional` | Можно использовать `run_task` с `task` или без него |
| `required` | Предпочтителен task-augmented `run_task` (инструмент всё равно указан под GraphQL для sync-клиентов) |

Опциональный sidecar (если подключаешь presentation через
[`demo/registry.presentation.json`](../demo/registry.presentation.json)):

- `demo/specs/presentation.json` — `build_presentation`, `deploy_presentation`

Обнаружение через GraphQL:

```graphql
{ search(q: "build") { server tool readOnly taskSupport description } }
```

`taskSupport` равен `optional` / `required` для инструментов из allowlist, иначе null.

---

## Аргументы `run_task`

```json
{
  "server": "presentation",
  "tool": "build_presentation",
  "arguments": { "target": "pdf" }
}
```

- `(server, tool)` не из allowlist → ошибка (`isError` на sync path или
  invalid-params при постановке в очередь).
- Sync path: шлюз вызывает upstream и возвращает его `CallToolResult`.
- Async path: клиент добавляет MCP `task` в `tools/call` → `CreateTaskResult`
  (`taskId`, `status: working`, `ttl`, `pollInterval`).

### Async JSON-RPC flow

1. `tools/call` `run_task` + `params.task` → `CreateTaskResult`
2. Опрашивайте `tasks/get` `{ taskId }` до `completed` / `failed` / `cancelled`
   (учитывайте `pollInterval`)
3. `tasks/result` `{ taskId }` → исходный `CallToolResult` (блокирует до
   terminal-состояния, если вызвано рано)
4. Опционально: `tasks/list`, `tasks/cancel`

Server capability при подключении: `tasks.list`, `tasks.cancel`,
`tasks.requests.tools.call`. Только **`run_task`** может дополняться task — не
`query_graphql`.

---

## SQLite layout

Файл: `tasks.db_path` (по умолчанию `state/tasks.db`).

| Колонка | Роль |
| ------ | ---- |
| `task_id` | UUID primary key |
| `owner` | Привязка контекста (phase 1: `"anon"`) |
| `server` / `tool` | Целевой upstream |
| `status` | `working` / `input_required` / `completed` / `failed` / `cancelled` |
| `result_json` | Сериализованный `CallToolResult`, когда terminal |
| `ttl_ms` / `poll_interval_ms` | Объявляется клиентам |
| `created_at` / `last_updated_at` | ISO-8601 для MCP `Task` |
| `created_unix_ms` | GC |

Переживает перезапуск шлюза: клиенты могут вызывать `tasks/get` /
`tasks/result` по строкам в SQLite. `ttl_ms` / `poll_interval_ms`
**объявляются** клиентам в `CreateTaskResult`; метод `TaskStore::gc()` есть,
но фоновый GC в runtime пока **не запущен** (в отличие от recorder idle GC).
In-process waiters используют `Notify`; после перезапуска `tasks/result`
опрашивает SQLite.

---

## Когда что использовать

| Сценарий | Инструмент |
| -------- | ---- |
| Пакетные короткие чтения / discovery | `query_graphql` (один документ, aliases) |
| Долгий процесс, sync-only клиент (например Cursor + progress) | `query_graphql` mutation **или** `run_task` без `task` |
| Долгий процесс, task-aware клиент | `run_task` + `task: {}` |

run_task рассчитан только на долгие upstream-инструменты из allowlist. Discovery-запросы (servers, search) и любые крошечные чтения туда не входят — они отдаются мгновенно и не являются задачами. Попытка вызвать их через run_task вернёт ошибку (isError на sync path либо invalid-params при постановке в очередь), а оборачивать мгновенное чтение в механизм задач (строка в SQLite, поллинг, TTL) попросту бессмысленно. Такие операции выполняйте через query_graphql.

---

## Тесты и покрытие

| Проверка | Команда / файл |
| ----- | -------------- |
| E2E SEP-1686 | `cargo test -p vmcp --test run_task_tasks` |
| Unit TaskStore / TaskRunner | `cargo test -p vmcp-server --lib tasks::` |
| Unit skills load / render / CRUD | `cargo test -p vmcp-server --lib skills::` |
| Line coverage gate (≥93%, включает `tasks` + `sessions` + `skills` + prompt agg) | `cargo llvm-cov -p vmcp-server --lib --fail-under-lines 93 --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'` |

Также см. [skills.md](skills.md#tests--coverage),
[aggregation.md](aggregation.md) и
[builds-and-modes.md](builds-and-modes.md#optional-native-mcp-tasks-run_task).