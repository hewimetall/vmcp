# AGENTS.md

**Language:** English | [Русский](AGENT.ru.md)

## Cursor Cloud instructions

### Product overview

**vmcp** is a Rust workspace for a virtual MCP gateway that exposes upstream
MCP servers through the `query_graphql` GraphQL tool over streamable HTTP at
`/mcp`. The optional `[tasks]` configuration adds SEP-1686 `run_task` support
with a SQLite TaskStore; see [`docs/tasks.md`](docs/tasks.md). The optional
`[mcp].latest` setting advertises protocol version `2026-07-28` alongside the
legacy version (`2025-11-25`). It is disabled by default; see
[`docs/mcp-2026-07-28.md`](docs/mcp-2026-07-28.md). Optional Python benchmarks
live in `bench/` and do not require a running gateway.

### Toolchain

- **Rust:** the workspace specifies `rust-version = "1.89"` (tested with 1.96;
  the Docker builder uses 1.95). Before the first build, run:
  `rustup install stable && rustup default stable`.
- **Demo:** [`demo/README.md`](demo/README.md) and its
  [`demo/vmcp.toml`](demo/vmcp.toml) configuration.

### Build, test, and lint

Usage is documented in `README.md`. Run these standard commands from the
repository root:

| Task | Command |
| ---- | ------- |
| Build | `cargo build --workspace` |
| Unit tests | `cargo test --workspace --lib` |
| Admin coverage | `cargo llvm-cov -p vmcp-admin --lib --fail-under-lines 99 --ignore-filename-regex '(integration|ui_regression|pages)\.rs'` |
| Server coverage | `cargo llvm-cov -p vmcp-server --lib --fail-under-lines 96 --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'` |
| Coverage CI | `.github/workflows/ci.yml` — CI enforces the coverage gates listed above. |
| Sessions | JSON in `[recorder].sessions_dir` (`.registry/` plus dumps); see [`docs/sessions.md`](docs/sessions.md) |
| Upstream registry | `registry.json`, sidecars, and skills/prompts; see [`docs/upstreams.md`](docs/upstreams.md) |
| Skills / prompts | YAML in `skills_dir`, plus upstream prompts through `[proxy]`; see [`docs/skills.md`](docs/skills.md) |
| DCR clients | SQLite at `[auth].clients_db_path` (persists across restarts); see [`docs/authentication.md`](docs/authentication.md#dcr-clients-survive-restart) |
| Full test suite | `cargo test --workspace` |
| Format check | `cargo fmt --all --check` |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` (see `.github/workflows/ci.yml`) |

### Demo

```bash
cargo run -p vmcp -- --config ./demo/vmcp.toml
# or: ./vmcp --config ./demo/vmcp.toml
```

[`demo/vmcp.toml`](demo/vmcp.toml) already defines `registry_path`, `spec_dir`,
`skills_dir`, `[upstream]` timeouts, and `auth.enabled = false`. Do not set only
part of the `VMCP_UPSTREAM__*` environment configuration: it replaces the
entire `[upstream]` block.

Smoke test: `VMCP_BIN=./vmcp python3 demo/smoke_demo_gateway.py`.

### mcp-presentation via stdio (optional)

[`demo/registry.presentation.json`](demo/registry.presentation.json) runs the
presentation server over stdio instead of HTTP. Point `command` to the virtual
environment binary from a local checkout (Python ≥ 3.14, `uv sync`), then
temporarily replace the registry in the configuration:

```bash
# in a copy of demo/vmcp.toml:
#   registry_path = "./demo/registry.presentation.json"
#   lock_path     = "./demo/presentation/tools.lock.json"
cargo run -p vmcp -- --config ./demo/vmcp.toml
```

Alternatively, override the demo configuration with environment variables:

```bash
VMCP_REGISTRY_PATH=./demo/registry.presentation.json \
VMCP_LOCK_PATH=./demo/presentation/tools.lock.json \
cargo run -p vmcp -- --config ./demo/vmcp.toml
```

### Authentication for local API testing

OAuth is enabled in the root [`vmcp.toml`](vmcp.toml), with master password
`demo-master`. Authentication is disabled in the demo configuration.

For authenticated scripted calls, issue a static bearer token:

```bash
cargo run -p vmcp -- pre-reg --name demo --scope mcp:use --out /tmp/tokens.json
# in TOML: [auth] tokens_file = "/tmp/tokens.json"
# or: VMCP_AUTH__TOKENS_FILE=/tmp/tokens.json
```

MCP over HTTP uses SSE (`Accept: application/json, text/event-stream`). After
`initialize`, include the `Mcp-Session-Id` response header in subsequent
requests.

To disable authentication in the root configuration for local use only, set
`auth.enabled = false` or `VMCP_AUTH__ENABLED=false`.

### Optional components

- **bench/** — see [`docs/bench.md`](docs/bench.md).
  Run `cd bench && uv sync && uv run python run.py ...`; real LLM runs require
  `OPENAI_API_KEY`.
- **Local stdio host** — vmcp is an HTTP-only gateway; for pipe-based hosts,
  use [vmcp-lite](https://github.com/hewimetall/vmcp-lite) (`uvx
  vmcp-lite-mcp`).

### Implementation notes

- The `read_only` and `task_support` sidecar fields determine whether GraphQL
  fields are exposed under Query or Mutation.
- The repository includes `docker-compose.yml`, `docker-compose.build.yml`,
  and a `Dockerfile`.
