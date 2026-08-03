# Реестр сессий и записи

HTTP-сессии шлюза (вкладка **Sessions** в админке) хранятся в виде JSON-файлов на диске. Поэтому при перезапуске шлюза список сессий не теряется: она снова читается с диска.

Конфигурация: секция `[recorder]` в [`vmcp.toml`](../vmcp.toml).

```toml
[recorder]
sessions_dir     = "./sessions"   # создаётся автоматически
redact_keys      = ["password","secret","token","api_key","Authorization"]
idle_ttl_secs    = 300            # закрыть idle + tear-down rmcp-сессии (FD/SSE)
gc_interval_secs = 30
```

Переменные окружения:
- `VMCP_RECORDER__SESSIONS_DIR=/var/lib/vmcp/sessions`
- `VMCP_SESSION_CHANNEL_CAPACITY=<n>` — окно mpsc на Streamable HTTP сессию
  (если не задано — дефолт rmcp)

Idle GC не только помечает запись в registry как `closed`, но и вызывает
`LocalSessionManager::close_session` на `/mcp` и `/mcp-proxy`, чтобы
транспортные воркеры и их дескрипторы не копились после шторма
пересоздания сессий. `idle_ttl_secs` также задаёт rmcp `keep_alive`.

SSE GET / resume с `Mcp-Session-Id` делает `registry.touch` (без
`request_count`), чтобы idle GC не рвал живой стрим, пока rmcp
`keep_alive` ещё сбрасывается на transport-событиях.

---

## Раскладка на диске

```text
sessions/                          # recorder.sessions_dir
  .registry/
    <session_id>.json              # SessionRegistry (переживает перезапуск)
  <client_id>/
    <session_id>.jsonl             # дамп обмена JSON-RPC / MCP
    <session_id>.meta.json         # метаданные дампа для слияния в admin UI
```

| Путь | Роль |
| ---- | ---- |
| `.registry/{id}.json` | Живая запись registry: client, счётчики, `active` / `closed` |
| `{client}/{id}.jsonl` | Append-only запись обмена (чувствительные ключи отредактированы) |
| `{client}/{id}.meta.json` | Сводка дампа (`started_at`, `request_count`, `upstream`, …) |

`SessionRegistry::open(sessions_dir)` при запуске загружает каждый `.registry/*.json`.
`record_request` / `close` / idle GC атомарно перезаписывают соответствующий файл (`.json.tmp` → rename).

Повреждённые, нечитаемые JSON-файлы или файлы с некорректным id пропускаются с предупреждением.

---

## Что переживает перезапуск

| Данные | Переживают? |
| ---- | --------- |
| Записи registry (`.registry/`) | **Да** — перезагружаются; статус сохраняется до срабатывания idle GC |
| Дампы обмена (`.jsonl` / `.meta.json`) | **Да** |
| DCR OAuth `client_id` + уникальный `name` | **Да** — SQLite `auth.clients_db_path` ([authentication.md](authentication.md#dcr-clients-survive-restart)) |
| Активный MCP-транспорт / rmcp-сессия | **Нет** — клиент должен переподключиться |
| OAuth JWT access-токены | **Нет** — нужен повторный consent; либо используйте статические `pre-reg` токены |

Очистка «повисших» дампов при старте. Если процесс, писавший дамп обмена, аварийно завершился посреди сессии, его meta-файл (.meta.json) остаётся в статусе active, хотя запись давно прекратилась. Поэтому при каждом запуске recorder выполняет startup_cleanup: находит такие застрявшие meta-файлы и переводит их в closed. Эта процедура работает только с дампами и не затрагивает записи .registry/.

---

## Admin UI

`GET /admin/api/sessions` объединяет:

1. DCR / pre-reg clients (каждый с уникальным операторским `name`)
2. Живой снимок `SessionRegistry` (из `.registry/`)
3. Дамповые meta-файлы на диске в подкаталогах клиентов

Переименовать client можно из колонки Sessions (поле ввода) или через
`PATCH /admin/api/sessions/:client_id` с телом `{"name":"…"}`.

Монтируйте `sessions_dir` как **writable** volume в Docker, чтобы registry и дампы сохранялись при пересоздании контейнера.

---

## Покрытие

`sessions.rs` включён в llvm-cov gate для vmcp-server вместе с
`skills.rs`, `tasks.rs` и модулями prompt aggregation (порог **93%** line coverage):

```bash
cargo llvm-cov -p vmcp-server --lib --fail-under-lines 93 \
  --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'
```

Юнит-тесты: `cargo test -p vmcp-server --lib sessions::`.

См. также [skills.md](skills.md#tests--coverage),
[clients.md](clients.md#admin-ui), [deployment.md](deployment.md),
[builds-and-modes.md](builds-and-modes.md).