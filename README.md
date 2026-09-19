# vmcp

**Language:** English | [Русский](README.ru.md)

[![coverage](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/hewimetall/vmcp/main/docs/badges/coverage.json)](https://github.com/hewimetall/vmcp/actions/workflows/ci.yml)

An MCP gateway written in Rust. It aggregates multiple upstream MCP servers behind a single GraphQL endpoint, allowing an agent to make one `query_graphql` call instead of many round trips.

## Why vmcp

- **One `query_graphql` tool** — send GraphQL and vmcp routes requests to the upstream servers. Reads run in parallel; writes run sequentially.
- **Dynamic schema** — built at startup from upstream `tools/list` responses.
- **Tasks (optional)** — run long-running tools as durable tasks backed by SQLite.
- **OAuth 2.1 + PKCE + DCR** — or static bearer tokens.
- **Hot reload** — tokens, `registry.json`, and prompts update without a restart.
- **GCF output (optional)** — `[gql].gcf` for `query_graphql` (`/mcp`) and `[proxy].gcf` for `/mcp-proxy`. Use [GCF](https://gcformat.com/) instead of JSON.
- **`/api/v1`** — operator API for token CRUD and upstream reloads (Bearer `mcp:admin`).

## Getting started

```bash
docker pull ghcr.io/hewimetall/vmcp:1.0.0
./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0
```

Or run the binary directly:

```bash
curl -fsSL -o vmcp.tgz "https://github.com/hewimetall/vmcp/releases/download/v1.0.0/vmcp-1.0.0-linux-x86_64.tar.gz"
tar -xzf vmcp.tgz
./vmcp --config ./demo/vmcp.toml
```

The gateway listens on `http://127.0.0.1:8765`:
- `/mcp` — MCP endpoint
- `/health` — liveness endpoint
- OAuth endpoints (`/authorize`, `/token`, `/register`, …) — authentication is disabled in the demo

Demo: [`demo/README.md`](demo/README.md).

## Configuration

Edit `vmcp.toml`. Any key can be overridden with a `VMCP_*` environment variable; use `__` for nested keys:

```bash
VMCP_AUTH__MASTER_PASSWORD_ARGON2='$argon2id$...' cargo run -p vmcp
# Advertise MCP 2026-07-28 (off by default): VMCP_MCP__LATEST=true
```

Utilities:

```bash
cargo run -p vmcp -- init
cargo run -p vmcp -- add mcp --transport http notion https://mcp.notion.com/mcp
cargo run -p vmcp -- add mcp --transport stdio time -- uvx mcp-server-time
# (probe tools/list → specs/<name>.json; offline: --no-spec)
cargo run -p vmcp -- add tool presentation build_presentation --task-support optional
cargo run -p vmcp -- add skill search_docs --description 'docs' --template 'Call query_graphql…'
cargo run -p vmcp -- add tasks
cargo run -p vmcp -- list mcp
cargo run -p vmcp -- list tool
cargo run -p vmcp -- hash-password --password 'secret'
cargo run -p vmcp -- print-config
```

### Static tokens (for CI)

OAuth issues short-lived JWTs that become invalid after a restart. For CI and demos, use a non-expiring bearer token:

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

The file is hot-reloaded. **Removing a line revokes that token.** These are unrestricted tokens with no expiration, so store them as secrets.

### Disable authentication (local use only)

`auth.enabled = false` removes bearer-token protection from `/mcp` and hides `/admin`. **Never use this setting in production.**

### Local stdio (Claude Desktop, Cursor)

vmcp supports HTTP only. For stdio, use [vmcp-lite](https://github.com/hewimetall/vmcp-lite):

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

## Build

```bash
cargo build --release -p vmcp                        # + admin UI (default)
cargo build --release -p vmcp --no-default-features  # without admin UI
```

## Crates

| Crate | Purpose |
| ----- | ------- |
| `vmcp` | Entry-point binary (axum + rmcp). |
| `vmcp-config` | Configuration (figment + TOML + environment variables). |
| `vmcp-registry` | `registry.json`, specs, and lock file. |
| `vmcp-upstream` | Pool of upstream MCP clients. |
| `vmcp-graphql` | Dynamic GraphQL schema. |
| `vmcp-auth` | OAuth 2.1 + PKCE + DCR and JWKS. |
| `vmcp-server` | MCP surface, tasks, and skills. |
| `vmcp-notify` | Notification ring buffer. |
| `vmcp-admin` | Admin UI and recordings. |
| `vmcp-watch` | File watcher and hot reload. |

## Documentation

See [`docs/README.md`](docs/README.md) for the full guide to deployment, authentication, upstream servers, tasks, skills, and clients.

## License

MIT — see [LICENSE](LICENSE).
