# Builds and runtime modes

**Language:** English | [Русский](ru/builds-and-modes.md)

## Cargo features

The `vmcp` binary is a single crate with compile-time features:

| Feature | Default | Includes |
| ------- | ------- | -------- |
| `admin` | yes | `/admin` SPA |

The following subcommands are available in **all** builds: `hash-password`, `pre-reg`, and `print-config`.

```bash
cargo build --release -p vmcp                        # + admin (default)
cargo build --release -p vmcp --no-default-features  # without admin

docker build --target runtime -t vmcp:latest .
docker build --target runtime --build-arg FEATURES="--no-default-features" -t vmcp:no-admin .
```

On a VPS, pull the prebuilt image from `ghcr.io/hewimetall/vmcp` through [`deploy/bootstrap.sh`](../deploy/bootstrap.sh). See [deployment.md](deployment.md).

---

## HTTP gateway

**Command:** `vmcp` or `vmcp serve --config vmcp.toml`

| Path | Description |
| ---- | ----------- |
| `/mcp` | `query_graphql` (+ `run_task` when `[tasks]` is enabled); skill discovery through `prompts` / `searchPrompts` / `getPrompt` |
| `/mcp-proxy` | Optional transparent proxy for upstream `tools/*` + `prompts/*` (`[proxy] enabled = true`) |
| `/admin` | Operator console (the `admin` feature) |
| `/health` | Liveness |
| `/ready` | Readiness (soft upstream checks) |
| `/api/v1/*` | Operator JSON API (Bearer `mcp:admin`) |

Use it for remote clients (Cursor/Claude/HTTP), multiple OAuth clients, and the admin UI with session recording.

Sessions and dumps are stored as JSON in `[recorder].sessions_dir`; mount this directory in production. See [sessions.md](sessions.md).

### Minimal production configuration

```toml
host = "0.0.0.0"
port = 8765
public_base_url = "https://example.com"
registry_path = "/data/registry.json"

[auth]
issuer = "https://example.com"
master_password_argon2 = "$argon2id$..."
```

---

## Local stdio → vmcp-lite

vmcp supports HTTP only. For stdio hosts such as Claude Desktop or a Cursor pipe, use [vmcp-lite](https://github.com/hewimetall/vmcp-lite) (`uvx vmcp-lite-mcp`). Setup instructions: [clients.md](clients.md#local-stdio-host--vmcp-lite).

---

## No authentication (development)

```toml
[auth]
enabled = false
```

This bypasses OAuth/bearer authentication and prevents `/admin` from being mounted. **Never use this on an untrusted network.**

---

## Configuration precedence

1. `vmcp.toml` (or `--config` / `VMCP_CONFIG`)
2. `VMCP_*` environment variables, with `__` for nesting (override TOML)

| TOML key | Environment variable |
| -------- | -------------------- |
| `host` | `VMCP_HOST` |
| `auth.master_password_argon2` | `VMCP_AUTH__MASTER_PASSWORD_ARGON2` |
| `upstream.spawn_timeout_ms` | `VMCP_UPSTREAM__SPAWN_TIMEOUT_MS` |
| `auth.enabled` | `VMCP_AUTH__ENABLED` |
| `gql.gcf` | `VMCP_GQL__GCF` |
| `proxy.gcf` | `VMCP_PROXY__GCF` |

Print the effective configuration with `vmcp print-config`.

---

## Upstream registry

For the complete guide, see [upstreams.md](upstreams.md).

vmcp loads upstreams from `registry.json`, resolves tools through `tools/list` plus sidecars, and exposes skill YAML (and upstream prompts when `[proxy]` is enabled) as MCP prompts.

```
demo/
  vmcp.toml          # ready-to-use configuration (paths, timeouts, auth off)
  registry.json      # upstream definitions
  specs/             # sidecar JSON per upstream
  skills/            # YAML playbooks → MCP prompts (may be empty)
  stand/             # sample Python + C4 docs for filesystem / architect / LSP
```

Local demo environment: [`demo/README.md`](../demo/README.md).

```bash
cargo run -p vmcp -- --config ./demo/vmcp.toml
# or: ./vmcp --config ./demo/vmcp.toml
```

---

## Optional native MCP Tasks (`run_task`)

For the complete guide, see [tasks.md](tasks.md).

```toml
[tasks]
enabled = true
db_path = "state/tasks.db"
task_ttl_ms = 300000
poll_interval_ms = 2000
max_concurrent = 16
```

When this is enabled and a tool has `execution.taskSupport` (or a sidecar sets `task_support: optional/required`):

- the **`run_task`** tool is registered
- the **`tasks`** capability is advertised (`get` / `result` / `list` / `cancel`)
- tasks are stored in **SQLite** (`db_path`, WAL, survives restarts)

GraphQL (`query_graphql`) remains **synchronous**. `run_task` exposes only allowlisted, task-capable tools. It is disabled by default.

Environment variables: `VMCP_TASKS__ENABLED=true`, `VMCP_TASKS__DB_PATH=…`.

---

## Latest MCP (`2026-07-28`, optional)

By default, the gateway advertises only legacy revisions (through `2025-11-25`).
The `[mcp].latest` flag adds `2026-07-28` to `supportedVersions`. This follows
the same pattern as AgentCore's `supportedVersions`: the version is a gateway
property, and the client selects it on each request.

```toml
[mcp]
latest = true
```

Environment variable: `VMCP_MCP__LATEST=true`.

`latest = false` (the default) is the safe path: `server/discover` and
`initialize` do not advertise 2026. `latest = true` is dual-era
(legacy + latest), not modern-only. For details and limitations, including
listen and recorder SID behavior, see [mcp-2026-07-28.md](mcp-2026-07-28.md).

---

## GCF output (optional, two flags)

[GCF](https://gcformat.com/) uses the generic profile instead of JSON in MCP tool text. The flags are **independent**, and both are off by default. If the encoder rejects a value (an integer outside the i64 range), vmcp falls back to JSON.

| Flag | Environment variable | Applies to |
| ---- | -------------------- | ---------- |
| `[gql].gcf` | `VMCP_GQL__GCF` | `/mcp` `query_graphql` envelope |
| `[proxy].gcf` | `VMCP_PROXY__GCF` | `/mcp-proxy` `{server}__{tool}` results |

```toml
[gql]
gcf = true          # GraphQL /mcp only

[proxy]
enabled = true
gcf = true          # /mcp-proxy only; does not require [gql].gcf
```

On `/mcp-proxy`, JSON text or `structuredContent` is converted to GCF; plain-text errors are left unchanged. When the flag is enabled, `tools/list` and the server instructions state that the response uses GCF.

---

## Tests and coverage

The server gate is ≥96% (skills, prompt aggregation, tasks, sessions):

```bash
cargo llvm-cov -p vmcp-server --lib --fail-under-lines 96 \
  --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'
```

The exclusions match exact basenames (`otel_file.rs`, `proxy.rs`, `lib.rs`, `recorder.rs`). The regex is anchored to the path so it does not match `prompt_proxy.rs`.

CI workflow [`ci.yml`](../.github/workflows/ci.yml): `vmcp-server` (≥96%), `vmcp-admin` (≥99%), and `vmcp-auth` (≥93%) coverage gates on pushes to `main` / `cursor/**` and on pull requests.
