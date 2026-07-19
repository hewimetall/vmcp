# Demo

Мини-стенд: gateway + несколько MCP upstreams над проектом [`stand/`](stand/).
Конфиг: [`vmcp.toml`](vmcp.toml).

## Что внутри

| Upstream | Зачем |
|----------|--------|
| `time` | время / таймзоны |
| `filesystem` | файлы в `stand/` |
| `architect_c4` | C4 в `stand/docs/` |
| `agent_lsp` | LSP по Python в `stand/` |
| `context7` | доки библиотек (нужен `CONTEXT7_API_KEY`) |

## Нужно на машине

- бинарь `vmcp` (release с GitHub) **или** `cargo run -p vmcp`
- `uv` (для `uvx`)
- Node.js / `npx`
- `agent-lsp` + `pyright` (`npm i -g pyright`)

## Поднять

Из корня репозитория:

```bash
# опционально для context7:
# export CONTEXT7_API_KEY=...

./vmcp --config ./demo/vmcp.toml
# или:
cargo run -p vmcp -- --config ./demo/vmcp.toml
```

Проверка:

```bash
curl -fsS http://127.0.0.1:8765/health
# ok
```

Автопроверка:

```bash
VMCP_BIN=./vmcp python3 demo/smoke_demo_gateway.py
```

## Что вызвать

Auth выключен в `demo/vmcp.toml`. MCP: `http://127.0.0.1:8765/mcp`, tool `query_graphql`.

Список серверов:

```graphql
{ servers { name toolCount } }
```

Время:

```graphql
{
  time {
    getCurrentTime(timezone: "Europe/Moscow") { json }
  }
}
```

Файл из стенда:

```graphql
{
  filesystem {
    readTextFile(path: "src/main.py") { json text }
  }
}
```

C4-модель:

```graphql
{
  architectC4 {
    getModel { json }
  }
}
```

LSP (сначала корень проекта — абсолютный путь):

```graphql
mutation {
  agentLsp {
    startLsp(rootDir: "/ABS/PATH/TO/new_vmcp/demo/stand") { json }
  }
}
```

```graphql
{
  agentLsp {
    listSymbols(filePath: "/ABS/PATH/TO/new_vmcp/demo/stand/src/main.py") { json }
  }
}
```

Context7 (если есть ключ):

```graphql
{
  context7 {
    resolveLibraryId(libraryName: "react") { json }
  }
}
```

Если какой-то upstream не поднялся, gateway всё равно работает — в `servers` его просто не будет.
