# Регистрация upstream-сервисов

Как регистрировать MCP-бэкенды и как их **tools** и **prompts** появляются на `/mcp` и `/mcp-proxy`.

При запуске шлюз:

1. Грузит **`registry.json`** → запускает/подключает каждый upstream
2. Вызывает **`tools/list`** (+ опц. **`prompts/list`**)
3. Мёржит **sidecar**-переопределения → пишет **`tools.lock.json`**
4. Собирает GraphQL-схему + грузит YAML-**skills** как MCP-промпты

| Ключ конфига | Default | Роль |
| --- | ------- | ---- |
| `registry_path` | `./registry.json` | Каталог upstream-сервисов |
| `spec_dir` | `./specs` | Sidecar JSON |
| `lock_path` | `./tools.lock.json` | Снимок tools при boot (аудит/drift) |
| `skills_dir` | `./skills` | YAML → MCP-промпты |
| `[upstream].spawn_timeout_ms` | `30000` | Бюджет запуска |
| `[upstream].call_timeout_ms` | `60000` | Бюджет одного вызова |
| `[proxy].enabled` | off | Upstream-промпты + `/mcp-proxy` |

Env: `VMCP_REGISTRY_PATH`, `VMCP_SPEC_DIR`, `VMCP_LOCK_PATH`, `VMCP_SKILLS_DIR`, …
Демо-стенд: [`demo/vmcp.toml`](../demo/vmcp.toml) + [`demo/README.md`](../demo/README.md)
(paths и `[upstream]` timeouts уже в toml — не задавай частичный `VMCP_UPSTREAM__*`).

---

## 1. Upstream-сервисы (`registry.json`)

Правится вручную. Нет файла → пустой пул (шлюз стартует). Единственный ключ списка — **`upstreams`** (legacy `servers` в 1.0 = ошибка парсинга).

```json
{
  "upstreams": [
    {
      "name": "presentation",
      "description": "MCP presentation builder (PDF/web).",
      "transport": "http",
      "url": "http://127.0.0.1:8001/mcp",
      "enabled": true,
      "sidecar_spec": "presentation.json"
    }
  ]
}
```

**Общие поля:** `name` (обязателен → GraphQL namespace + proxy-префикс), `description`, `transport` (`stdio` default / `http`), `enabled` (default true), `sidecar_spec`.

**stdio:** `command`, `args`, `env` (`${VAR}` раскрывается), `cwd`.
**http:** `url`, `bearer` (raw token → `Authorization: Bearer …`).

```json
{
  "name": "tavily",
  "transport": "http",
  "url": "https://mcp.tavily.com/mcp/",
  "bearer": "${TAVILY_API_KEY}",
  "enabled": true
}
```

`${ENV}` раскрывается в `url`/`bearer`/`env` (секреты не в git; неустановленные → пустая строка).

**При запуске:** все upstream стартуют параллельно; упавший логируется, шлюз продолжает с частичным пулом. Медленный `npx`/venv → подними `spawn_timeout_ms`.

---

## 2. Инструменты (авторезолв)

Tools не нужно перечислять вручную — vmcp вызывает `tools/list` и строит из него GraphQL-поля.

```
tools/list → CachedTool → sidecar overrides → ResolvedTool → GraphQL + tools.lock.json
```

| Источник | Эффект |
| ------ | ------ |
| `readOnlyHint: true` | → **`Query.<server>`** (параллельно) |
| отсутствует / `false` | → **`Mutation.<server>`** (последовательно, безопаснее) |
| `execution.taskSupport` | В allowlist `run_task` (если `[tasks]`) |
| Sidecar | Переопределяет `read_only`/`description`/`task_support` |

Агрегация: [aggregation.md](aggregation.md). Allowlist задач: [tasks.md](tasks.md#which-tools-appear-on-run_task).

### Sidecar specs (`spec_dir`)

Опциональный JSON — когда аннотации upstream отсутствуют/неверны (частая беда сторонних пакетов).

```json
{
  "server": "presentation",
  "tools": [
    { "name": "list_sessions", "read_only": true },
    { "name": "build_presentation", "read_only": false, "task_support": "optional" }
  ]
}
```

Путь = `spec_dir` + filename (или абсолютный). Записи без совпадения с живым tool игнорятся (фантомы не создаются).

### `tools.lock.json`

Снимок tools после мёржа (name, schema, `read_only`, `task_support`) — baseline для `detect_drift` (изменение только description ≠ drift). Перезаписывается при каждом запуске — **вручную не редактируй в проде**.

### Где появляются tools

| Поверхность | Форма |
| ------- | ------ |
| `/mcp` → `query_graphql` | `Query.<server>.<tool>` / `Mutation.<server>.<tool>` |
| `/mcp` → `run_task` | Только task-capable из allowlist (`[tasks]`) |
| `/mcp-proxy` | Плоские `{server}__{tool}` (`[proxy]`) |

### Hot-swap / watchers

| Что | Механизм | Поведение |
| --- | -------- | --------- |
| GraphQL schema | `ArcSwap` + `swap_schema` | Атомарная подмена |
| Skills | Admin CRUD → reload | Без рестарта |
| Static tokens | `vmcp-watch` на `tokens_file` | Hot-reload после rename |
| Upstream prompts | `prompts/list_changed` → `refresh_prompts` | Кэш + forward клиентам |
| Upstream tools | `tools/list_changed` | Пока forward клиентам; `detect_drift`/`swap_schema` — задел под drift-handler |

`vmcp-watch` — общий file-watcher (parent dir + фильтр по имени, переживает tmp→rename). Сейчас висит на `tokens_file`; тот же примитив — под registry/skills.

---

## 3. Промпты

| Источник | Регистрация | Имена | Всегда? |
| ------ | ---------------- | ----- | ---------- |
| **Локальные skills** | YAML в `skills_dir` | голое `name` | Да |
| **Upstream-промпты** | upstream `prompts/list` | `{server}__{prompt}` | Только `[proxy] enabled` |

### Локальные skills

YAML-playbook → MCP `prompts/*` + GraphQL `prompts`/`getPrompt`/`searchPrompts`. Полная схема + Admin CRUD: [skills.md](skills.md).

```yaml
name: search_docs
description: Look up library docs via Context7.
arguments:
  - name: library
    required: true
template: |
  Call query_graphql with:
  { context7 { resolveLibraryId(libraryName: "{{library}}") { json } } }
```

- `name` без `__` (зарезервировано под upstream-префикс)
- Диск читается **при запуске** — после ручных правок перезапускай
- Admin-вкладка **Skills** — CRUD с hot-swap без рестарта

### Upstream-промпты

Забираются при старте (`prompts/list`). Нет capability / пустой список → продолжаем без них.

```toml
[proxy]
enabled = true
mcp_path = "/mcp-proxy"   # ≠ основному mcp_path
```

Default в коде `false`; в поставляемом `vmcp.toml` demo proxy **включён**. Флаг монтирует tools+prompts на `/mcp-proxy` и включает upstream-промпты в GraphQL на `/mcp`.

При `getPrompt` vmcp добавляет в начало **таблицу маршрутизации GraphQL-инструментов** (по возможности сужённую до упомянутых tools). Вызывай tools через `query_graphql` на `/mcp`.

`prompts/list_changed` обновляет кэш и форвардится клиентам (если объявлен `prompts.listChanged`).

---

## Чеклист: новый upstream end-to-end

1. **Сервис** — запись в `registry.json` (`stdio`/`http`)
2. **Sidecar** (опц.) — `specs/<name>.json` + `sidecar_spec`
3. **Skills** (опц.) — YAML, учит агента GraphQL-форме сервера
4. **Proxy** (опц.) — `[proxy] enabled = true` для `{server}__*` tools/prompts
5. **Tasks** (опц.) — `task_support` на долгих + `[tasks]` ([tasks.md](tasks.md))
6. Рестарт + проверка:

```bash
curl -fsS http://127.0.0.1:8765/health
# затем via MCP/GraphQL: { servers { name toolCount } } и { prompts { name source } }
```

---

## Связанные документы

| Тема | Документ |
| ----- | --- |
| Skill YAML + upstream-промпты | [skills.md](skills.md) |
| `run_task` / `task_support` | [tasks.md](tasks.md) |
| Агрегация Query vs Mutation | [aggregation.md](aggregation.md) |
| Режимы / конфиг | [builds-and-modes.md](builds-and-modes.md) |
| Production-монтирования | [deployment.md](deployment.md) |
| Discovery для клиентов | [clients.md](clients.md) |
