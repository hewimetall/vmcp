# Сборки и режимы запуска

## Cargo features

Бинарь `vmcp` — один crate с compile-time features:

| Feature | Default | Что включает |
| ------- | ------- | ------------ |
| `admin` | да | `/admin` SPA |

Подкоманды есть во **всех** сборках: `hash-password`, `pre-reg`, `print-config`.

```bash
cargo build --release -p vmcp                        # + admin (default)
cargo build --release -p vmcp --no-default-features  # без admin

docker build --target runtime -t vmcp:latest .
docker build --target runtime --build-arg FEATURES="--no-default-features" -t vmcp:no-admin .
```

На VPS — качай готовый image из `ghcr.io/hewimetall/vmcp` через [`deploy/bootstrap.sh`](../deploy/bootstrap.sh), см. [deployment.md](deployment.md).

---

## HTTP gateway

**Команда:** `vmcp` или `vmcp serve --config vmcp.toml`

| Path | Описание |
| ---- | -------- |
| `/mcp` | `query_graphql` (+ `run_task` при `[tasks]`); skill discovery через `prompts`/`searchPrompts`/`getPrompt` |
| `/mcp-proxy` | Опциональный transparent proxy upstream `tools/*` + `prompts/*` (`[proxy] enabled = true`) |
| `/admin` | Панель оператора (feature `admin`) |
| `/health` | Liveness |

Когда нужен: удалённые клиенты (Cursor/Claude/HTTP), несколько клиентов с OAuth, admin UI + запись сессий.

Сессии/dumps — JSON в `[recorder].sessions_dir` (в проде монтируй каталог), см. [sessions.md](sessions.md).

### Минимальный prod-конфиг

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

## Локальный stdio → vmcp-lite

vmcp — только HTTP. Для stdio-хостов (Claude Desktop, Cursor pipe) бери [vmcp-lite](https://github.com/hewimetall/vmcp-lite) (`uvx vmcp-lite-mcp`). Настройка — [clients.md](clients.md#локальный-stdio-host--vmcp-lite).

---

## Без auth (dev)

```toml
[auth]
enabled = false
```

Пропускает OAuth/bearer, `/admin` не монтируется. **Никогда в недоверенных сетях.**

---

## Конфигурация: порядок

1. `vmcp.toml` (или `--config` / `VMCP_CONFIG`)
2. Env `VMCP_*`, вложенность через `__` (переопределяет TOML)

| TOML key | Env |
| -------- | --- |
| `host` | `VMCP_HOST` |
| `auth.master_password_argon2` | `VMCP_AUTH__MASTER_PASSWORD_ARGON2` |
| `upstream.spawn_timeout_ms` | `VMCP_UPSTREAM__SPAWN_TIMEOUT_MS` |
| `auth.enabled` | `VMCP_AUTH__ENABLED` |

Итоговый конфиг: `vmcp print-config`.

---

## Upstream registry

Полное руководство: [upstreams.md](upstreams.md).

vmcp грузит upstreams из `registry.json`, резолвит tools через `tools/list` + sidecars, отдаёт skill YAML (и upstream prompts при `[proxy]`) как MCP prompts.

```
demo/
  vmcp.toml          # готовый конфиг (paths, timeouts, auth off)
  registry.json      # upstream definitions
  specs/             # sidecar JSON per upstream
  skills/            # YAML playbooks → MCP prompts (может быть пустым)
  stand/             # sample Python + C4 docs для filesystem / architect / LSP
```

Локальный демо-стенд: [`demo/README.md`](../demo/README.md).

```bash
cargo run -p vmcp -- --config ./demo/vmcp.toml
# или: ./vmcp --config ./demo/vmcp.toml
```

---

## Native MCP Tasks (`run_task`, опционально)

Полное руководство: [tasks.md](tasks.md).

```toml
[tasks]
enabled = true
db_path = "state/tasks.db"
task_ttl_ms = 300000
poll_interval_ms = 2000
max_concurrent = 16
```

Когда включено и есть tool с `execution.taskSupport` (или sidecar `task_support: optional/required`):

- регистрируется tool **`run_task`**
- объявляется capability **`tasks`** (`get`/`result`/`list`/`cancel`)
- tasks в **SQLite** (`db_path`, WAL, переживает restart)

GraphQL (`query_graphql`) остаётся **sync**. Через `run_task` — только allowlisted task-capable tools. По умолчанию выключено.

Env: `VMCP_TASKS__ENABLED=true`, `VMCP_TASKS__DB_PATH=…`.

---

## Tests / coverage

Server gate ≥93% (skills, prompt aggregation, tasks, sessions):

```bash
cargo llvm-cov -p vmcp-server --lib --fail-under-lines 93 \
  --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'
```

Игнор — точный basename (`otel_file.rs`, `proxy.rs`, `lib.rs`, `recorder.rs`); regex привязан к path, чтобы не задеть `prompt_proxy.rs`.

CI [`coverage.yml`](../.github/workflows/coverage.yml): server (≥93%) + admin (≥99%) gates на каждый PR, sticky comment, artifact `lcov.info`, обновление README badges.
