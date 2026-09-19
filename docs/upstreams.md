# Registering upstream services

**Language:** English | [Русский](ru/upstreams.md)

This guide explains how to register MCP backends and how their **tools** and **prompts** appear on `/mcp` and `/mcp-proxy`.

At startup, the gateway:

1. Loads **`registry.json`** and starts or connects to each upstream
2. Calls **`tools/list`** (and optionally **`prompts/list`**)
3. Merges **sidecar** overrides and writes **`tools.lock.json`**
4. Builds the GraphQL schema and loads YAML **skills** as MCP prompts

| Configuration key | Default | Purpose |
| ----------------- | ------- | ------- |
| `registry_path` | `./registry.json` | Upstream service catalog |
| `spec_dir` | `./specs` | Sidecar JSON directory |
| `lock_path` | `./tools.lock.json` | Snapshot of tools at startup (audit/drift) |
| `skills_dir` | `./skills` | YAML → MCP prompts |
| `[upstream].spawn_timeout_ms` | `30000` | Startup time budget |
| `[upstream].call_timeout_ms` | `60000` | Per-call time budget |
| `[proxy].enabled` | off | Upstream prompts + `/mcp-proxy` |
| `[proxy].gcf` | off | GCF instead of JSON on `/mcp-proxy` (independent of `[gql].gcf`) |

Environment variables: `VMCP_REGISTRY_PATH`, `VMCP_SPEC_DIR`, `VMCP_LOCK_PATH`, `VMCP_SKILLS_DIR`, …
Demo environment: [`demo/vmcp.toml`](../demo/vmcp.toml) + [`demo/README.md`](../demo/README.md).
Paths and `[upstream]` timeouts are already set in the TOML file; do not set only a subset of the `VMCP_UPSTREAM__*` variables.

### Operator constraints

| Topic | Behavior |
| ----- | -------- |
| Skills on disk | No hot reload; YAML edits require a restart or Admin Skills CRUD (G07) |
| Stdio in the cluster image | The runtime image has no Node/`uv`; use **HTTP** upstreams in Kubernetes (G08) |
| HA | One replica + RWO PVC; the pool and schema are in memory, so multiple replicas are **not** supported (G12) |
| Health | `connected` is updated from RPC outcomes; an idle, dead upstream may appear healthy for a long time if it receives no calls (G24) |
| `${ENV}` | A missing variable causes a **registry load error** (strict) |
| Registry size | Maximum **256** upstreams; duplicate `name` values are rejected |
| Catalog (G25) | An `upstream:<name>` allowlist filters `servers` / `search` / the GraphQL schema / `/mcp-proxy` `tools/list`. `mcp:admin` gets the full view. Contract: [ADR 0002](adr/0002-per-caller-catalog-visibility.md) |

---

## 1. Upstream services (`registry.json`)

Edit the registry manually or through the CLI:

```bash
vmcp init                                      # vmcp.toml + empty registry.json + directories
vmcp add mcp --transport http notion https://mcp.notion.com/mcp
vmcp add mcp --bearer '${API_KEY}' --transport http secure https://api.example.com/mcp
vmcp add mcp --transport stdio time -- uvx mcp-server-time
# ↑ connect + tools/list → writes specs/<name>.json and sidecar_spec (or use --no-spec)
vmcp add tool presentation build_presentation --task-support optional   # manual upsert
vmcp add skill search_docs --description 'docs' --template 'Call query_graphql…'
vmcp add tasks                                 # sets [tasks] enabled = true in vmcp.toml
vmcp list mcp|tool|skill
vmcp get mcp time
vmcp remove tool presentation build_presentation
```

`vmcp mcp add …` is an alias for `vmcp add mcp …`. Options such as `--transport`, `--env`, and `--bearer` must appear **before** the server name; for stdio, the command and arguments follow `--`. Paths come from `--config` / `VMCP_CONFIG`. `${ENV}` placeholders in the registry are not expanded when written; the environment variable must be set for the probe, or you must use `--no-spec`. A sidecar can be generated automatically from `tools/list` or created manually with `add tool`.

If the file does not exist, the gateway starts with an empty pool. The only valid list key is **`upstreams`**; the legacy `servers` key is a parse error in 1.0.

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

**Common fields:** `name` (required; becomes the GraphQL namespace and proxy prefix), `description`, `transport` (`stdio` by default or `http`), `enabled` (default true), and `sidecar_spec`.

**stdio:** `command`, `args`, `env` (`${VAR}` is expanded), `cwd`.
**http:** `url`, `bearer` (a raw token sent as `Authorization: Bearer …`; this is a **service** credential), `forward_identity` (default **`false`**).

### Caller identity → HTTP upstream (opt-in)

A mixed cluster can contain **external** SaaS services alongside **internal** adapters.

| Upstream | `forward_identity` | Reason |
| -------- | ------------------ | ------ |
| Notion / Context7 / … | `false` (default) | Do not expose the subject or groups externally |
| `stand-api-mcp` / cluster adapters | `true` | Let the adapter enforce tenancy using `X-Vmcp-*` |

```bash
# external: no identity by default
vmcp add mcp --transport http notion https://mcp.notion.com/mcp

# internal: explicitly enable identity forwarding
vmcp add mcp --transport http --forward-identity stand_api http://stand-api.svc/mcp
```

When `forward_identity = true` and the caller is known, `tools/call` receives:

| Header | Contents |
| ------ | -------- |
| `Authorization` | only the registry `bearer` (never the user's JWT) |
| `X-Vmcp-Subject` | subject |
| `X-Vmcp-Groups` | groups joined with `,` |
| `X-Vmcp-Client-Id` | client ID |
| `X-Vmcp-Scope` | MCP scopes |

These headers are set by **vmcp** after its own authentication checks, not by the client. Contract: [ADR 0001](adr/0001-forward-auth-trust-and-identity-propagation.md).

```json
{
  "name": "tavily",
  "transport": "http",
  "url": "https://mcp.tavily.com/mcp/",
  "bearer": "${TAVILY_API_KEY}",
  "enabled": true
}
```

`${ENV}` is expanded in `url`, `bearer`, and `env` (secrets stay out of Git; missing variables cause a **registry load error**).

**At startup:** all upstreams start in parallel. A failed upstream is logged, and the gateway continues with a partial pool. Increase `spawn_timeout_ms` for a slow `npx` process or virtual environment.

---

## 2. Tools (automatic resolution)

You do not need to list tools manually. vmcp calls `tools/list` and turns the result into GraphQL fields.

```
tools/list → CachedTool → sidecar overrides → ResolvedTool → GraphQL + tools.lock.json
```

| Source | Effect |
| ------ | ------ |
| `readOnlyHint: true` | → **`Query.<server>`** (parallel) |
| absent / `false` | → **`Mutation.<server>`** (sequential, safer) |
| `execution.taskSupport` | Included in the `run_task` allowlist when `[tasks]` is enabled |
| Sidecar | Overrides `read_only` / `description` / `task_support` |

For aggregation, see [aggregation.md](aggregation.md). For the task allowlist, see [tasks.md](tasks.md#tools-exposed-through-run_task).

### Sidecar specs (`spec_dir`)

Use optional JSON sidecars when upstream annotations are missing or incorrect, a common issue with third-party packages.

```json
{
  "server": "presentation",
  "tools": [
    { "name": "list_sessions", "read_only": true },
    { "name": "build_presentation", "read_only": false, "task_support": "optional" }
  ]
}
```

The path is the `spec_dir` plus the filename, unless it is absolute. Entries that do not match a live tool are ignored; they do not create phantom tools.

### `tools.lock.json`

This is a post-merge tool snapshot containing the name, schema, `read_only`, and `task_support`. It is the baseline for `detect_drift`; a description-only change is not considered drift. The file is overwritten at every startup, so **do not edit it manually in production**.

### Where tools appear

| Surface | Form |
| ------- | ---- |
| `/mcp` → `query_graphql` | `Query.<server>.<tool>` / `Mutation.<server>.<tool>` |
| `/mcp` → `run_task` | Only task-capable tools from the allowlist (`[tasks]`) |
| `/mcp-proxy` | Flat `{server}__{tool}` names (`[proxy]`) |

### Hot swap and watchers

| Resource | Mechanism | Behavior |
| -------- | --------- | -------- |
| GraphQL schema | `ArcSwap` + `swap_schema` | Atomic replacement |
| Skills | Admin CRUD → reload | No restart |
| Static tokens | `vmcp-watch` on `tokens_file` | Hot reload after rename |
| **Registry (`registry.json`)** | recursive watch + mtime poll + `POST /api/v1/upstreams/reload` | Add/remove/replace upstreams and rebuild the schema. With a Kubernetes ConfigMap, explicitly calling the reload API after apply is more reliable |
| Upstream prompts | `prompts/list_changed` → `refresh_prompts` | Cache and forward to clients |
| Upstream tools | `tools/list_changed` → `refresh_tools` + GraphQL rebuild | Cache, schema, and forward to clients |

`vmcp-watch` is the shared file watcher. It watches the parent directory, filters by filename, and survives a temporary-file-to-rename update. It watches both `tokens_file` and `registry_path`.

Operator status: `GET /api/v1/upstreams` (Bearer `mcp:admin`) returns `connected`, `tool_count`, `last_error`, and `last_ok_unix_ms`.<br>
Readiness probe: `GET /ready` is soft. If the registry contains enabled upstreams and none are `connected`, it returns 503; `/health` remains the liveness endpoint.<br>
The legacy `GET /admin/api/servers` endpoint is unchanged.

---

<a id="3-register-prompts"></a>
## 3. Prompts

| Source | Registration | Names | Always available? |
| ------ | ------------ | ----- | ----------------- |
| **Local skills** | YAML in `skills_dir` | bare `name` | Yes |
| **Upstream prompts** | upstream `prompts/list` | `{server}__{prompt}` | Only when `[proxy]` is enabled |

### Local skills

A YAML playbook becomes MCP `prompts/*` plus GraphQL `prompts` / `getPrompt` / `searchPrompts`. For the complete schema and Admin CRUD, see [skills.md](skills.md).

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

- `name` must not contain `__`, which is reserved for upstream prefixes
- Files on disk are read **at startup**; restart after making manual changes
- The **Skills** tab in the admin UI provides CRUD with hot swap and no restart

### Upstream prompts

These are fetched at startup with `prompts/list`. If the capability is absent or the list is empty, startup continues without them.

```toml
[proxy]
enabled = true
mcp_path = "/mcp-proxy"   # must differ from the main mcp_path
```

The default in code is `false`; the demo proxy is **enabled** in the supplied `vmcp.toml`. This flag mounts tools and prompts on `/mcp-proxy` and includes upstream prompts in GraphQL on `/mcp`.

For `getPrompt`, vmcp prepends a **GraphQL tool-routing table**, narrowed to the tools mentioned in the prompt when possible. Call the tools through `query_graphql` on `/mcp`.

`prompts/list_changed` updates the cache and is forwarded to clients when `prompts.listChanged` is declared.

---

## Checklist: new upstream end to end

1. **Service** — use `vmcp mcp add …` or add an entry to `registry.json` manually (`stdio`/`http`)
2. **Sidecar** (optional) — add `specs/<name>.json` + `sidecar_spec`
3. **Skills** (optional) — add YAML that teaches the agent the server's GraphQL shape
4. **Proxy** (optional) — set `[proxy] enabled = true` for `{server}__*` tools/prompts
5. **Tasks** (optional) — set `task_support` on long-running tools and enable `[tasks]` ([tasks.md](tasks.md))
6. Restart (or hot-reload the registry), then verify:

```bash
curl -fsS http://127.0.0.1:8765/health
# then through MCP/GraphQL: { servers { name toolCount } } and { prompts { name source } }
```

---

## Related documentation

| Topic | Document |
| ----- | -------- |
| Skill YAML + upstream prompts | [skills.md](skills.md) |
| `run_task` / `task_support` | [tasks.md](tasks.md) |
| Query vs Mutation aggregation | [aggregation.md](aggregation.md) |
| Modes / configuration | [builds-and-modes.md](builds-and-modes.md) |
| Production mounts | [deployment.md](deployment.md) |
| Client discovery | [clients.md](clients.md) |
