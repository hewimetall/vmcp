# AGENTS.md

## Инструкции для Cursor Cloud

### Обзор продукта

**vmcp** — Rust workspace (виртуальный MCP gateway), который предоставляет
upstream MCP-серверы через GraphQL tool `query_graphql` по streamable HTTP на
`/mcp`. Опциональный `[tasks]` добавляет SEP-1686 `run_task` с SQLite TaskStore
— см. [`docs/tasks.md`](docs/tasks.md). Опциональные Python benchmarks находятся
в `bench/` и не требуют запущенного gateway.

### Инструментарий

- **Rust:** workspace `rust-version = "1.89"` (проверено на 1.96; Docker
  builder — 1.95). Перед первой сборкой:
  `rustup install stable && rustup default stable`.
- **Demo:** [`demo/README.md`](demo/README.md) + конфиг [`demo/vmcp.toml`](demo/vmcp.toml).

### Сборка, тесты, линт

Использование описано в `README.md`. Стандартные команды из корня репозитория:

| Задача | Команда |
| ------ | ------- |
| Сборка | `cargo build --workspace` |
| Юнит-тесты | `cargo test --workspace --lib` |
| Admin coverage | `cargo llvm-cov -p vmcp-admin --lib --fail-under-lines 99 --ignore-filename-regex '(integration|ui_regression|pages)\.rs'` |
| Server coverage | `cargo llvm-cov -p vmcp-server --lib --fail-under-lines 93 --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'` |
| Coverage CI | `.github/workflows/coverage.yml` — gates выше + PR comment + LCOV + README % badges (`docs/badges/`) |
| Sessions | JSON в `[recorder].sessions_dir` (`.registry/` + dumps) — см. [`docs/sessions.md`](docs/sessions.md) |
| Upstream registry | `registry.json` + sidecars + skills/prompts — см. [`docs/upstreams.md`](docs/upstreams.md) |
| Skills / prompts | YAML в `skills_dir` + upstream через `[proxy]` — см. [`docs/skills.md`](docs/skills.md) |
| DCR clients | SQLite в `[auth].clients_db_path` (сохраняется после restart) — см. [`docs/authentication.md`](docs/authentication.md#dcr-clients-survive-restart) |
| Полные тесты | `cargo test --workspace` |
| Проверка format | `cargo fmt --all --check` |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` (см. `.github/workflows/ci.yml`) |

### Demo

```bash
cargo run -p vmcp -- --config ./demo/vmcp.toml
# или: ./vmcp --config ./demo/vmcp.toml
```

В [`demo/vmcp.toml`](demo/vmcp.toml) уже заданы `registry_path` / `spec_dir` / `skills_dir`,
`[upstream]` timeouts и `auth.enabled = false`. Не подставляй частичный
`VMCP_UPSTREAM__*` env — он перетирает весь блок `[upstream]`.

Смоук: `VMCP_BIN=./vmcp python3 demo/smoke_demo_gateway.py`.

### mcp-presentation via stdio (optional)

[`demo/registry.presentation.json`](demo/registry.presentation.json) — presentation
через stdio вместо HTTP. Направь `command` на venv binary из локального checkout
(Python ≥ 3.14, `uv sync`), затем временно подмени registry в конфиге:

```bash
# в копии demo/vmcp.toml:
#   registry_path = "./demo/registry.presentation.json"
#   lock_path     = "./demo/presentation/tools.lock.json"
cargo run -p vmcp -- --config ./demo/vmcp.toml
```

Или одним env поверх demo-конфига:

```bash
VMCP_REGISTRY_PATH=./demo/registry.presentation.json \
VMCP_LOCK_PATH=./demo/presentation/tools.lock.json \
cargo run -p vmcp -- --config ./demo/vmcp.toml
```

### Auth для локального API testing

Корневой [`vmcp.toml`](vmcp.toml): OAuth включён (master password `demo-master`).
Демо-конфиг: auth выключен.

Для scripted calls с auth выпусти static bearer:

```bash
cargo run -p vmcp -- pre-reg --name demo --scope mcp:use --out /tmp/tokens.json
# в toml: [auth] tokens_file = "/tmp/tokens.json"
# или: VMCP_AUTH__TOKENS_FILE=/tmp/tokens.json
```

MCP over HTTP использует SSE (`Accept: application/json, text/event-stream`).
После `initialize` передавай response header `Mcp-Session-Id` в последующих
requests.

Отключить auth в корневом конфиге (только локально): `auth.enabled = false`
или `VMCP_AUTH__ENABLED=false`.

### Опциональные компоненты

- **bench/** — см. [`docs/bench.md`](docs/bench.md).
  `cd bench && uv sync && uv run python run.py ...` (для реальных LLM runs
  нужен `OPENAI_API_KEY`).
- **Локальный stdio host** — vmcp здесь только HTTP gateway; для pipe hosts
  используйте [vmcp-lite](https://github.com/hewimetall/vmcp-lite) (`uvx
  vmcp-lite-mcp`).

### Особенности

- Sidecar `read_only` / `task_support` режут GraphQL на Query vs Mutation.
- Есть `docker-compose.yml` (+ `docker-compose.build.yml`) и `Dockerfile`.
