# Skills (MCP-промпты)

Skills — это готовые playbook'и в формате YAML, которые пишет оператор. Они лежат в каталоге `skills_dir`. При запуске vmcp читает оттуда все файлы `*.yaml` / `*.yml` и публикует их как MCP **prompts** (`prompts/list` / `prompts/get`).

Клиент (Claude Code, Cursor и т. п.) подставляет готовый текст промпта как сообщение пользователя. Смысл: дать модели готовый сценарий, вместо того чтобы она сама искала нужные инструменты.

Как это связано с upstream-**сервисами** и **инструментами** — см. **[upstreams.md](upstreams.md#3-register-prompts)**.

**Где настраивается** — ключ `skills_dir` в [`vmcp.toml`](../vmcp.toml)
(в демо — [`demo/vmcp.toml`](../demo/vmcp.toml)):

```toml
skills_dir = "./skills"
```

Переопределение: `VMCP_SKILLS_DIR=…`. Каталог демо: [`demo/skills/`](../demo/skills/)
(можно положить свои `*.yaml`; пустой каталог — нормально).

---

## Как выглядит YAML

```yaml
name: search_docs            # ^[a-z0-9_-]{1,64}$; без `__`
description: |
  Короткое описание, которое видно в prompts/list — по нему агент
  решает, вызывать ли этот skill.
arguments:
  - name: library
    required: true
    description: Имя библиотеки…
  - name: topic
    required: false
    default: "getting started"  # нельзя вместе с required: true
template: |
  Call query_graphql with:
  { context7 { resolveLibraryId(libraryName: "{{library}}") { json } } }
```

| Поле | Правила |
| ----- | ----- |
| `name` | Обязательно; слаг `^[a-z0-9_-]{1,64}$`; без `__` (это зарезервировано для upstream-имён `{server}__{prompt}`) |
| `description` | Обязательно; не пустое |
| `required` и `default` у аргумента | Нельзя указывать одновременно |
| `template` | Обязательно; не пустой; синтаксис Handlebars |

Шаблоны — это **Handlebars в нестрогом режиме**, поэтому необязательных аргументов может и не быть. Для условий используйте `{{#if var}}…{{/if}}`. Своих helper'ов нет.

---

## Как skills загружаются

Функция `load_skills(dir)` ведёт себя так:

- **Нет каталога** → пустой список (шлюз всё равно запустится).
- **Не-YAML файлы и подкаталоги** → пропускаются.
- **Битый YAML, пустое имя, пустое описание, неверное имя или `__` в имени** → запись в лог и пропуск.
- **Одинаковый `name` в двух файлах** → второй файл пропускается.
- Skills читаются **один раз при запуске**. Hot-reload нет — после правок перезапустите vmcp.

Вкладка **Skills** в Admin UI умеет создавать, менять и удалять YAML через `save_skill` / `delete_skill`. Запись атомарная: сначала `.<name>.yaml.tmp`, потом переименование.

---

## Как skills обнаруживаются

Skills — первая ступень «ленивой лестницы» discovery:

1. GraphQL `{ prompts { name description source } }` / `{ getPrompt(name) { text } }` (предпочтительно во время работы) — или MCP `prompts/list` / `prompts/get`.
2. `{ servers { … } }` / `{ search(q) }` / `{ searchPrompts(q) }`.
3. `__type(name: "Query"|"Mutation"|…)`.
4. `query_graphql` / (опционально) `run_task`.

**Локальные YAML-skills** всегда видны под своими обычными именами на `/mcp` (и в GraphQL, и в MCP prompts).

**Upstream MCP prompts** появляются только при `[proxy] enabled = true`:

| Поверхность | Path | Что даёт |
| ------- | ---- | ------------ |
| GraphQL | `/mcp` → `prompts` / `searchPrompts` / `getPrompt` | Доступ во время работы через `query_graphql` |
| MCP proxy | `/mcp-proxy` | Нативные `tools/*` + `prompts/*` (с префиксом `{server}__{name}`) |

Имена upstream-промптов имеют вид `{server}__{prompt}`. Когда вы делаете `get`, vmcp добавляет в начало ответа таблицу маршрутизации GraphQL-инструментов (по возможности только те инструменты, что упомянуты в теле промпта; иначе весь upstream-каталог). Вызывайте инструменты через `query_graphql` на `/mcp`, а не по сырым MCP-именам (сырой `tools/call` на `/mcp-proxy` тоже остаётся доступным).

Уведомление `notifications/prompts/list_changed` от upstream обновляет кэш промптов этого сервера и пересылается клиентам `/mcp` (если у них объявлен `prompts.listChanged`).

Аргументы в GraphQL `getPrompt` и в `/mcp-proxy` `prompts/get` приводятся к строкам (так требует контракт MCP `prompts/get`): числа и bool превращаются в `"10"` / `"true"`.

Подробнее — [clients.md](clients.md#использование-graphql-tool) и [aggregation.md](aggregation.md).

---

## Как включить upstream-промпты

```toml
[proxy]
enabled = true
mcp_path = "/mcp-proxy"   # должен отличаться от основного mcp_path (/mcp)
```

Когда прокси выключен (так по умолчанию в пустой конфигурации): локальные skills продолжают работать, но upstream-промптов нет в GraphQL, а `/mcp-proxy` не монтируется.

---

## Тесты и покрытие

В gate по покрытию (llvm-cov) для `vmcp-server` входят: `skills.rs`, `prompt_catalog.rs`, `prompt_proxy.rs` (helpers) и `graphql_inject.rs` — вместе с `sessions.rs` и `tasks.rs`. Порог — **93% покрытия строк**.

Файл `proxy.rs` (HTTP-монтирование и wiring `ProxyServer`) остаётся **исключённым** из gate — как и `otel_file`, `lib`, `recorder`. Но prompt-хелперы, которые он вызывает, gate проходят.

```bash
# Unit-тесты
cargo test -p vmcp-server --lib skills::
cargo test -p vmcp-server --lib prompt_catalog::
cargo test -p vmcp-server --lib prompt_proxy::
cargo test -p vmcp-server --lib graphql_inject::

# Gate по покрытию (sessions + tasks + skills + агрегация промптов)
cargo llvm-cov -p vmcp-server --lib --fail-under-lines 93 \
  --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'
```

| Что проверяем | Команда |
| ----- | ------- |
| Загрузка / рендер / сохранение / удаление | `cargo test -p vmcp-server --lib skills::` |
| Каталог промптов / инъекция / нормализация | три команды `prompt_catalog::` / `prompt_proxy::` / `graphql_inject::` выше |
| Порог покрытия строк | команда выше |

Исключены из gate (много интеграции / mount wiring): `otel_file.rs`, `proxy.rs`, `lib.rs`, `recorder.rs`. Regex привязан к границам имени, поэтому `prompt_proxy.rs` под исключение **не** попадает.

См. также [tasks.md](tasks.md#tests--coverage), [sessions.md](sessions.md#coverage), [builds-and-modes.md](builds-and-modes.md).