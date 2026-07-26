# vmcp

[![coverage](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/hewimetall/vmcp/main/docs/badges/coverage.json)](https://github.com/hewimetall/vmcp/actions/workflows/coverage.yml)

MCP gateway на Rust. Собирает несколько upstream MCP-серверов в один GraphQL endpoint. Agent делает один вызов `query_graphql` вместо кучи round-trips.

## Зачем

- **Один tool `query_graphql`** — шлёшь GraphQL, vmcp разводит запросы по upstreams. Read — параллельно, write — последовательно.
- **Dynamic schema** — строится при старте из upstream `tools/list`.
- **Tasks (опционально)** — long-running tools как durable tasks на SQLite.
- **OAuth 2.1 + PKCE + DCR** — или static bearer tokens.
- **Hot-reload** — токены, `registry.json` и промпты обновляются без рестарта.
- **`/api/v1`** — operator Token CRUD + upstreams reload (Bearer `mcp:admin`).

## Старт

```bash
docker pull ghcr.io/hewimetall/vmcp:1.0.0
./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0
```

Или из бинарника:

```bash
curl -fsSL -o vmcp.tgz "https://github.com/hewimetall/vmcp/releases/download/v1.0.0/vmcp-1.0.0-linux-x86_64.tar.gz"
tar -xzf vmcp.tgz
./vmcp --config ./demo/vmcp.toml
```

Слушает `http://127.0.0.1:8765`:
- `/mcp` — MCP endpoint
- `/health` — liveness
- OAuth surface (`/authorize`, `/token`, `/register`, …) — в демо auth выключен

Демо: [`demo/README.md`](demo/README.md).

## Конфиг

Правишь `vmcp.toml`. Любой ключ переопределяется через `VMCP_*` env (nested — через `__`):

```bash
VMCP_AUTH__MASTER_PASSWORD_ARGON2='$argon2id$...' cargo run -p vmcp
```

Утилиты:

```bash
cargo run -p vmcp -- hash-password --password 'secret'
cargo run -p vmcp -- print-config
```

### Static tokens (для CI)

OAuth выдаёт короткоживущие JWT, которые дохнут после рестарта. Для CI/demo используй бессрочный bearer:

```bash
cargo run -p vmcp -- pre-reg --name ci --scope mcp:use --out ./tokens.json
# → vmcp_xK3v...
```

```toml
[auth]
tokens_file = "./tokens.json"
```

```bash
curl -H "Authorization: Bearer vmcp_xK3v..." http://127.0.0.1:8765/mcp
```

Файл hot-reload'ится. **Удалил строку = отозвал токен.** Это god-key без expiry — храни как secret.

### Отключить auth (только локально)

`auth.enabled = false` — снимает bearer с `/mcp` и прячет `/admin`. **Никогда в проде.**

### Локальный stdio (Claude Desktop, Cursor)

vmcp — только HTTP. Для stdio используй [vmcp-lite](https://github.com/hewimetall/vmcp-lite):

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

## Сборка

```bash
cargo build --release -p vmcp                        # + admin UI (default)
cargo build --release -p vmcp --no-default-features  # без admin UI
```

## Crates

| Crate | Назначение |
| ----- | ---------- |
| `vmcp` | Entry binary (axum + rmcp). |
| `vmcp-config` | Config (figment + TOML + env). |
| `vmcp-registry` | `registry.json`, specs, lock. |
| `vmcp-upstream` | Пул upstream MCP-клиентов. |
| `vmcp-graphql` | Dynamic GraphQL schema. |
| `vmcp-auth` | OAuth 2.1 + PKCE + DCR, JWKS. |
| `vmcp-server` | MCP surface, tasks, skills. |
| `vmcp-notify` | Notification ring buffer. |
| `vmcp-admin` | Admin UI + recordings. |
| `vmcp-watch` | File watcher, hot-reload. |

## Документация

Полное руководство: [`docs/README.md`](docs/README.md) — deployment, auth, upstreams, tasks, skills, clients.

## Лицензия

MIT — см. [LICENSE](LICENSE).
