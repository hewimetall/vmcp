# Native MCP Tasks (`run_task`)

**Language:** English | [Русский](ru/tasks.md)

vmcp can expose **SEP-1686** (the 2025-11-25 specification revision) for
long-running upstream tools without turning every GraphQL call into a task.

| Path | Client experience | Who waits |
| ---- | ----------------- | --------- |
| `query_graphql` | Immediate GraphQL JSON | The **gateway** waits for the upstream (sync) |
| `run_task` without `task` | Immediate `CallToolResult` | The **gateway** waits for one upstream tool |
| `run_task` with `task: {}` | Immediate `CreateTaskResult` | The **client** polls `tasks/get` / `tasks/result` |

Durable task rows are stored in **embedded SQLite** (WAL).

### Client logs (MCP logging)

During the `run_task` lifecycle (enqueue / call / completed / failed), vmcp
publishes `notifications/message` with `logger = "tasks/<task_id>"` to the
internal bus. These are the same messages forwarded to connected `/mcp`
clients (with the `logging` capability). Do not confuse this with the admin
session dump: listen to MCP logging for long-running tasks.

---

## Enabling Tasks

Tasks are disabled by default. Configure them in `vmcp.toml`:

```toml
[tasks]
enabled = true
db_path = "state/tasks.db"   # created automatically (parent dirs too)
task_ttl_ms = 300000         # retention advertised on CreateTaskResult
poll_interval_ms = 2000      # hint for clients
max_concurrent = 16          # in-flight upstream proxies
```

Environment overrides (figment):

```bash
VMCP_TASKS__ENABLED=true
VMCP_TASKS__DB_PATH=/var/lib/vmcp/tasks.db
VMCP_TASKS__MAX_CONCURRENT=8
```

At startup, if `enabled` is set but **no** upstream tool supports tasks, vmcp
logs a warning and **does not** register the `run_task` / `tasks` capability.

---

## Tools exposed through `run_task`

Only tools marked as task-capable are added to the allowlist:

1. Upstream `tools/list` → `execution.taskSupport` = `optional` | `required`
2. A sidecar override in `spec_dir/<server>.json`:

```json
{
  "server": "presentation",
  "tools": [
    { "name": "build_presentation", "read_only": false, "task_support": "optional" },
    { "name": "deploy_presentation", "read_only": false, "task_support": "optional" }
  ]
}
```

| `task_support` | Meaning |
| -------------- | ------- |
| omitted / `forbidden` | GraphQL only (not in `run_task`) |
| `optional` | `run_task` can be used with or without `task` |
| `required` | Task-augmented `run_task` is preferred (the tool is still listed under GraphQL for sync clients) |

An optional sidecar is available when presentation is connected through
[`demo/registry.presentation.json`](../demo/registry.presentation.json):

- `demo/specs/presentation.json` — `build_presentation`, `deploy_presentation`

Discover these tools through GraphQL:

```graphql
{ search(q: "build") { server tool readOnly taskSupport description } }
```

`taskSupport` is `optional` / `required` for allowlisted tools and null for
all other tools.

---

## `run_task` arguments

```json
{
  "server": "presentation",
  "tool": "build_presentation",
  "arguments": { "target": "pdf" }
}
```

- A `(server, tool)` pair outside the allowlist produces an error (`isError`
  on the sync path or invalid-params while enqueueing).
- On the sync path, the gateway invokes the upstream and returns its
  `CallToolResult`.
- On the async path, the client adds MCP `task` to `tools/call`, which returns
  a `CreateTaskResult` (`taskId`, `status: working`, `ttl`, `pollInterval`).

### Async JSON-RPC flow

1. `tools/call` `run_task` + `params.task` → `CreateTaskResult`
2. Poll `tasks/get` `{ taskId }` until `completed` / `failed` / `cancelled`
   (honor `pollInterval`)
3. `tasks/result` `{ taskId }` → the original `CallToolResult` (blocks until a
   terminal state if called early)
4. Optional: `tasks/list`, `tasks/cancel`

Server capability at connection time: `tasks.list`, `tasks.cancel`,
`tasks.requests.tools.call`. Only **`run_task`** can be task-augmented, not
`query_graphql`.

---

## SQLite layout

File: `tasks.db_path` (`state/tasks.db` by default).

| Column | Role |
| ------ | ---- |
| `task_id` | UUID primary key |
| `owner` | Context binding (phase 1: `"anon"`) |
| `server` / `tool` | Target upstream |
| `status` | `working` / `input_required` / `completed` / `failed` / `cancelled` |
| `result_json` | Serialized `CallToolResult` when terminal |
| `ttl_ms` / `poll_interval_ms` | Advertised to clients |
| `created_at` / `last_updated_at` | ISO-8601 values for MCP `Task` |
| `created_unix_ms` | GC |

The rows survive gateway restarts: clients can call `tasks/get` /
`tasks/result` for tasks stored in SQLite. `ttl_ms` / `poll_interval_ms` are
**advertised** to clients in `CreateTaskResult`; the `TaskStore::gc()` method
exists, but the runtime **does not yet start** background GC (unlike recorder
idle GC). In-process waiters use `Notify`; after a restart, `tasks/result`
polls SQLite.

---

## Choosing the right path

| Scenario | Tool |
| -------- | ---- |
| Batched short reads / discovery | `query_graphql` (one document, aliases) |
| Long-running process, sync-only client (for example, Cursor + progress) | `query_graphql` mutation **or** `run_task` without `task` |
| Long-running process, task-aware client | `run_task` + `task: {}` |

run_task is only intended for long-running, allowlisted upstream tools.
Discovery queries (servers, search) and other tiny reads do not belong there:
they return immediately and are not tasks. Calling them through run_task
returns an error (isError on the sync path or invalid-params while
enqueueing), and wrapping an immediate read in task machinery (a SQLite row,
polling, and a TTL) serves no purpose. Perform these operations through
query_graphql.

---

## Tests and coverage

| Check | Command / file |
| ----- | -------------- |
| E2E SEP-1686 | `cargo test -p vmcp --test run_task_tasks` |
| Unit TaskStore / TaskRunner | `cargo test -p vmcp-server --lib tasks::` |
| Unit skills load / render / CRUD | `cargo test -p vmcp-server --lib skills::` |
| Line coverage gate (≥96%, includes `tasks` + `sessions` + `skills` + prompt aggregation) | `cargo llvm-cov -p vmcp-server --lib --fail-under-lines 96 --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'` |

See also [skills.md](skills.md#tests--coverage),
[aggregation.md](aggregation.md), and
[builds-and-modes.md](builds-and-modes.md#optional-native-mcp-tasks-run_task).
