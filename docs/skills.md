# Skills (MCP prompts)

**Language:** English | [Русский](ru/skills.md)

Skills are ready-to-use YAML playbooks authored by the operator. They live in the `skills_dir` directory. At startup, vmcp reads every `*.yaml` / `*.yml` file there and publishes it as an MCP **prompt** (`prompts/list` / `prompts/get`).

The client (Claude Code, Cursor, and so on) inserts the rendered prompt text as a user message. The goal is to give the model a ready-made workflow instead of making it discover the required tools on its own.

For how this relates to upstream **services** and **tools**, see **[upstreams.md](upstreams.md#3-register-prompts)**.

**Configuration:** the `skills_dir` key in [`vmcp.toml`](../vmcp.toml)
([`demo/vmcp.toml`](../demo/vmcp.toml) in the demo):

```toml
skills_dir = "./skills"
```

Override it with `VMCP_SKILLS_DIR=…`. The demo directory is [`demo/skills/`](../demo/skills/).
You can add your own `*.yaml` files; an empty directory is valid.

---

## YAML format

```yaml
name: search_docs            # ^[a-z0-9_-]{1,64}$; must not contain `__`
description: |
  A short description shown in prompts/list; the agent uses it
  to decide whether to invoke this skill.
arguments:
  - name: library
    required: true
    description: Library name…
  - name: topic
    required: false
    default: "getting started"  # cannot be combined with required: true
template: |
  Call query_graphql with:
  { context7 { resolveLibraryId(libraryName: "{{library}}") { json } } }
```

| Field | Rules |
| ----- | ----- |
| `name` | Required; must be a `^[a-z0-9_-]{1,64}$` slug and must not contain `__` (reserved for upstream names in the form `{server}__{prompt}`) |
| `description` | Required and non-empty |
| An argument's `required` and `default` fields | Cannot be specified together |
| `template` | Required and non-empty; uses Handlebars syntax |

Templates use **Handlebars in non-strict mode**, so optional arguments may be absent. Use `{{#if var}}…{{/if}}` for conditionals. No custom helpers are available.

---

## How skills are loaded

`load_skills(dir)` behaves as follows:

- **Missing directory** → an empty list (the gateway still starts).
- **Non-YAML files and subdirectories** → skipped.
- **Invalid YAML, an empty name or description, an invalid name, or `__` in the name** → logged and skipped.
- **The same `name` in two files** → the second file is skipped.
- Skills are read **once at startup**. There is no hot reload; restart vmcp after making changes.

The **Skills** tab in the Admin UI can create, update, and delete YAML through `save_skill` / `delete_skill`. Writes are atomic: vmcp first writes `.<name>.yaml.tmp`, then renames it.

---

## How skills are discovered

Skills are the first step in the “lazy discovery ladder”:

1. GraphQL `{ prompts { name description source } }` / `{ getPrompt(name) { text } }` (preferred at runtime), or MCP `prompts/list` / `prompts/get`.
2. `{ servers { … } }` / `{ search(q) }` / `{ searchPrompts(q) }`.
3. `__type(name: "Query"|"Mutation"|…)`.
4. `query_graphql` / optionally `run_task`.

**Local YAML skills** are always available under their regular names on `/mcp`, through both GraphQL and MCP prompts.

**Upstream MCP prompts** are available only when `[proxy] enabled = true`:

| Surface | Path | What it provides |
| ------- | ---- | ---------------- |
| GraphQL | `/mcp` → `prompts` / `searchPrompts` / `getPrompt` | Runtime access through `query_graphql` |
| MCP proxy | `/mcp-proxy` | Native `tools/*` + `prompts/*`, prefixed with `{server}__{name}` |

Upstream prompt names have the form `{server}__{prompt}`. When you fetch a prompt, vmcp prepends a routing table of GraphQL tools to the response. Where possible, it includes only the tools mentioned in the prompt body; otherwise, it includes the entire upstream catalog. Invoke tools through `query_graphql` on `/mcp`, not by their raw MCP names. Raw `tools/call` requests on `/mcp-proxy` remain available as well.

An upstream `notifications/prompts/list_changed` notification refreshes that server's prompt cache and is forwarded to `/mcp` clients that declare `prompts.listChanged`.

Arguments to GraphQL `getPrompt` and `/mcp-proxy` `prompts/get` are coerced to strings, as required by the MCP `prompts/get` contract: numbers and booleans become `"10"` / `"true"`.

For details, see [clients.md](clients.md#using-the-graphql-tool) and [aggregation.md](aggregation.md).

---

## Enabling upstream prompts

```toml
[proxy]
enabled = true
mcp_path = "/mcp-proxy"   # must differ from the primary mcp_path (/mcp)
```

When the proxy is disabled, as it is by default in an empty configuration, local skills continue to work, but upstream prompts are unavailable in GraphQL and `/mcp-proxy` is not mounted.

---

## Tests and coverage

The llvm-cov coverage gate for `vmcp-server` includes `skills.rs`, `prompt_catalog.rs`, `prompt_proxy.rs` (helpers), and `graphql_inject.rs`, along with `sessions.rs` and `tasks.rs`. The threshold is **96% line coverage**.

`proxy.rs`, which handles HTTP mounting and `ProxyServer` wiring, remains **excluded** from the gate, as do `otel_file`, `lib`, and `recorder`. The prompt helpers it calls are covered by the gate.

```bash
# Unit tests
cargo test -p vmcp-server --lib skills::
cargo test -p vmcp-server --lib prompt_catalog::
cargo test -p vmcp-server --lib prompt_proxy::
cargo test -p vmcp-server --lib graphql_inject::

# Coverage gate (sessions + tasks + skills + prompt aggregation)
cargo llvm-cov -p vmcp-server --lib --fail-under-lines 96 \
  --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'
```

| What is tested | Command |
| -------------- | ------- |
| Loading / rendering / saving / deletion | `cargo test -p vmcp-server --lib skills::` |
| Prompt catalog / injection / normalization | The three `prompt_catalog::` / `prompt_proxy::` / `graphql_inject::` commands above |
| Line-coverage threshold | The command above |

The following files are excluded from the gate because they contain substantial integration or mount wiring: `otel_file.rs`, `proxy.rs`, `lib.rs`, and `recorder.rs`. The regex is anchored to the filename boundaries, so **`prompt_proxy.rs` is not excluded**.

See also [tasks.md](tasks.md#tests--coverage), [sessions.md](sessions.md#coverage), and [builds-and-modes.md](builds-and-modes.md).