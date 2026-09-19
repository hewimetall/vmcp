# Connecting clients

**Language:** English | [Русский](ru/clients.md)

## Client how-to guides

| Client | Guide |
| ------ | ----- |
| Cursor | [How to connect Cursor to vmcp](how-to/connect-cursor.md) |
| OpenCode | [How to connect OpenCode to vmcp](how-to/connect-opencode.md) |
| Agno | [How to connect an Agno agent to vmcp](how-to/connect-agno.md) |

Each guide includes prerequisites, a complete configuration or runnable
example, verification steps, and troubleshooting.

---

## Cursor / VS Code MCP (HTTP + OAuth)

1. Deploy vmcp at a public HTTPS URL (see [deployment.md](deployment.md)).
2. Add a **remote HTTP server** in Cursor's MCP settings:
   - URL: `https://<domain>/mcp`
3. On the first connection, Cursor completes DCR + PKCE and opens `/consent` in a browser. The gateway persists the DCR `client_id` in SQLite (`auth.clients_db_path`), so it remains valid after a restart; only the JWT needs to be refreshed.
4. Enter the **master password** in plain text, not its Argon2 hash.
5. Cursor stores the JWT and adds the `Authorization: Bearer …` header to requests to `/mcp`.

If OAuth fails immediately, check that:

- `public_base_url` matches the browser URL, including the scheme and host;
- `/.well-known/oauth-protected-resource` returns 200 (vmcp serves both the bare path and the `/mcp` variant);
- the master-password hash is present in the active configuration (`print-config`).

---

<a id="local-stdio-host--vmcp-lite"></a>
## Local stdio host → vmcp-lite

vmcp is an HTTP-only gateway. For local MCP hosts that communicate through stdin/stdout, such as Claude Desktop or a Cursor pipe, use the separate **[vmcp-lite](https://github.com/hewimetall/vmcp-lite)** project, which accepts input only through stdio.

Install it with `uvx vmcp-lite-mcp` or `pip install vmcp-lite-mcp` (the command is `vmcp-lite`). Add it to the host's `mcp.json`:

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

See `examples/demo` in the vmcp-lite repository for a demo and more details.

---

## curl / scripts (static token)

This is the preferred option for automation: unlike a JWT, a static token remains valid across gateway restarts.

```bash
TOKEN=$(cargo run -q -p vmcp -- pre-reg --name bot --out ./tokens.json)
# Add tokens_file to vmcp.toml, restart once, then:
curl -sS https://gateway.example.com/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

MCP Streamable HTTP uses SSE, so send `Accept: application/json, text/event-stream`. After `initialize`, read the `Mcp-Session-Id` response header and include it in subsequent requests.

---

## curl / scripts (full OAuth)

See the Bash example in [authentication.md](authentication.md#scripted-smoke-test). You will need the master password for the `POST /consent` step.

---

<a id="admin-ui"></a>
## Admin dashboard

`https://<domain>/admin` uses **HTTP Basic** authentication. The username can be any value, and the password is the master password in plain text. This is not a Bearer JWT or the `vmcp_…` token used on `/mcp`.

The dashboard shows upstream status and session recordings, provides a schema explorer and CRUD operations for skills, and compares `/mcp` with `/mcp-proxy`.

The session list and dumps are stored as JSON under `[recorder].sessions_dir` (`.registry/{id}.json` plus separate `.jsonl` / `.meta.json` files for each client) and **persist across gateway restarts**. For details, see **[sessions.md](sessions.md)**.

Skill playbooks (YAML in `skills_dir` → MCP `prompts/list` / `prompts/get`, as well as GraphQL `prompts` / `getPrompt`) are documented in **[skills.md](skills.md)**. Upstream prompts (`{server}__{name}`) require `[proxy]` (GraphQL on `/mcp` and MCP `prompts/*` on `/mcp-proxy`). For service, tool, and prompt registration, see **[upstreams.md](upstreams.md)**.

---

## Using the GraphQL tool

The primary tool is **`query_graphql`**. Pass it a GraphQL document. Use the following discovery order, or “ladder”:

1. `{ prompts { … } }` / `{ getPrompt(name) { text } }` (or MCP `prompts/list`) for skill playbooks ([skills.md](skills.md)).
2. `{ servers { name description toolCount readOnlyCount } }`.
3. `{ search(q: "time filesystem") { server tool readOnly taskSupport description } }` / `{ searchPrompts(q) { … } }`.
4. `__type(name: "Query")` / `__type(name: "Mutation")`.

Reads are aggregated in parallel; writes run sequentially, one upstream at a time. For details, see [aggregation.md](aggregation.md).

Demo: [`demo/README.md`](../demo/README.md) (`./vmcp --config ./demo/vmcp.toml`,
with authentication disabled). Perform writes through `mutation { <server> { … } }`.

---

## Long-running tools (`run_task`)

When `[tasks]` is enabled and upstreams declare `taskSupport`, clients also see the **`run_task`** tool (SEP-1686). Keep short and batched work on `query_graphql`; launch long-running work through `run_task`, either synchronously or with `task: {}` for asynchronous polling.

See **[tasks.md](tasks.md)** for the complete guide.

Cursor currently tends to use blocking `tools/call` requests with progress notifications, so either GraphQL or synchronous `run_task` is suitable. Task-aware hosts can run it asynchronously using the `task` field, then retrieve the result through `tasks/get` / `tasks/result`.

---

## Health check

```bash
curl -fsS https://<domain>/health    # → ok
```

Load balancers can access this path without authentication.