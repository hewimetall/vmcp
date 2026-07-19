# Подключение клиентов

## Cursor / VS Code MCP (HTTP + OAuth)

1. Разверните vmcp с публичным HTTPS-адресом (см. [deployment.md](deployment.md)).
2. В настройках Cursor MCP добавьте **удалённый HTTP-сервер**:
   - URL: `https://<domain>/mcp`
3. При первом подключении Cursor проходит DCR + PKCE и открывает `/consent` в браузере. Gateway сохраняет `client_id` из DCR в SQLite (`auth.clients_db_path`), поэтому после рестарта он продолжает работать — обновить нужно только JWT.
4. Введите **мастер-пароль** (открытым текстом, не argon2-хеш).
5. Cursor сохраняет JWT и добавляет заголовок `Authorization: Bearer …` при запросах на `/mcp`.

Если OAuth падает сразу, проверьте:

- `public_base_url` совпадает с адресом в браузере (схема + хост);
- `/.well-known/oauth-protected-resource` отвечает 200 (vmcp обслуживает и «голый» путь, и `/mcp`);
- хеш мастер-пароля в текущем конфиге (`print-config`).

---

## Локальный stdio-хост → vmcp-lite

vmcp — это только HTTP-gateway. Для локальных MCP-хостов, которые общаются через stdin/stdout (Claude Desktop, Cursor pipe), есть отдельный проект **[vmcp-lite](https://github.com/hewimetall/vmcp-lite)** — вход только через stdio.

Установка: `uvx vmcp-lite-mcp` или `pip install vmcp-lite-mcp` (команда `vmcp-lite`). Пропишите его в `mcp.json` хоста:

```json
{
  "mcpServers": {
    "vmcp-lite": {
      "command": "uvx",
      "args": ["vmcp-lite-mcp", "--config", "/path/to/vmcp.toml"]
    }
  }
}
```

Демо и подробности — в репозитории vmcp-lite (`examples/demo`).

---

## curl / скрипты (статический токен)

Лучший вариант для автоматизации: в отличие от JWT, он переживает рестарты gateway.

```bash
TOKEN=$(cargo run -q -p vmcp -- pre-reg --name bot --out ./tokens.json)
# добавьте tokens_file в vmcp.toml, перезапустите один раз, затем:
curl -sS https://gateway.example.com/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

MCP streamable HTTP работает через SSE, поэтому передавайте `Accept: application/json, text/event-stream`. После `initialize` берите заголовок ответа `Mcp-Session-Id` и повторяйте его в следующих запросах.

---

## curl / скрипты (полный OAuth)

См. bash-пример в [authentication.md](authentication.md#scripted-smoke-test). На шаге `POST /consent` понадобится мастер-пароль.

---

## Панель администратора

`https://<domain>/admin` — авторизация по **HTTP Basic** (имя пользователя любое, пароль = мастер-пароль открытым текстом). Это не Bearer JWT и не токен `vmcp_…`, который используется на `/mcp`.

Что даёт панель: статус upstream'ов, записи сессий, обозреватель схемы, CRUD для skills, сравнение `/mcp` и `/mcp-proxy`.

Список сессий и dumps хранятся в `[recorder].sessions_dir` как JSON в каталогах (`.registry/{id}.json` + отдельные `.jsonl` / `.meta.json` для каждого клиента) и **сохраняются после рестартов gateway**. Подробности: **[sessions.md](sessions.md)**.

Skill-playbook'и (YAML в `skills_dir` → MCP `prompts/list` / `prompts/get`, а также GraphQL `prompts` / `getPrompt`): **[skills.md](skills.md)**. Upstream-промпты (`{server}__{name}`) требуют `[proxy]` (GraphQL на `/mcp` и MCP `prompts/*` на `/mcp-proxy`). Регистрация services / tools / prompts: **[upstreams.md](upstreams.md)**.

---

## Использование GraphQL-инструмента

Основной инструмент — **`query_graphql`**: передайте ему GraphQL-документ. Порядок discovery («лестница»):

1. `{ prompts { … } }` / `{ getPrompt(name) { text } }` (или MCP `prompts/list`) — skill-playbook'и ([skills.md](skills.md)).
2. `{ servers { name description toolCount readOnlyCount } }`.
3. `{ search(q: "time filesystem") { server tool readOnly taskSupport description } }` / `{ searchPrompts(q) { … } }`.
4. `__type(name: "Query")` / `__type(name: "Mutation")`.

Чтения агрегируются параллельно; записи выполняются последовательно, по одному upstream за раз. Подробности: [aggregation.md](aggregation.md).

Демо: [`demo/README.md`](../demo/README.md) (`./vmcp --config ./demo/vmcp.toml`,
auth выключен). Записи — через `mutation { <server> { … } }`.

---

## Долгие инструменты (`run_task`)

Если включён `[tasks]` и upstream'ы объявляют `taskSupport`, клиенты дополнительно видят инструмент **`run_task`** (SEP-1686). Короткие и пакетные задачи оставляйте на `query_graphql`; долгие — запускайте через `run_task` (синхронно или с `task: {}` для асинхронного опроса).

Полное руководство: **[tasks.md](tasks.md)**.

Cursor сейчас обычно использует блокирующий tools/call с progress-уведомлениями — для него подойдёт GraphQL или синхронный run_task. Хосты с поддержкой задач могут запускать его в асинхронном режиме (поле task) и забирать результат через tasks/get / tasks/result.

---

## Проверка health

```bash
curl -fsS https://<domain>/health    # → ok
```

Балансировщики нагрузки могут обращаться к этому пути без авторизации.