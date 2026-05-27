# vmcp

A virtual MCP gateway that aggregates many upstream MCP servers behind a
single GraphQL endpoint.

Rust workspace, 9 crates. Speaks the Model Context Protocol over streamable
HTTP and exposes the union of all upstream tools as one GraphQL schema. An
agent makes one `query_graphql` call instead of N+1 round-trips across
individual MCP tools.

## What it does

- **One MCP tool, `query_graphql`** — clients send a GraphQL document, vmcp
  fans out to upstreams in parallel via aliased selection sets.
- **Lazy discovery ladder** — `prompts/list` (skill playbooks) →
  `{ servers }` → `{ search(q) }` → `__type(name)` → call. No deep
  `__schema` dumps.
- **Dynamic schema** built at boot from upstream `tools/list`. Each upstream
  `foo` gets `FooRead` under `Query.foo` and `FooWrite` under `Mutation.foo`,
  partitioned by `readOnlyHint`.
- **Hot swap on drift** — registered upstream tool set changes are detected
  and the schema is replaced atomically via `Arc<ArcSwap<Schema>>`.
- **OAuth 2.1 + PKCE + DCR** — argon2id master-password consent, locally
  rotated JWKS, JWT bearer on `/mcp`.

## Quick start

Requires Rust 1.80+ and Node (for the `npx` upstream in the demo registry).

```bash
cargo run -p vmcp
```

vmcp listens on `http://127.0.0.1:8765`:

- `/mcp` — MCP streamable HTTP endpoint (bearer-authenticated)
- `/health` — liveness probe, returns `ok`
- `/.well-known/oauth-authorization-server`, `/authorize`, `/consent`,
  `/token`, `/register`, `/.well-known/jwks.json` — OAuth surface

The default `demo/registry.json` spawns one upstream
(`@agentmemory/mcp` via `npx`). Point your MCP client at
`http://127.0.0.1:8765/mcp` and complete the OAuth flow; the default master
password is `demo-master` (rotate before deploying anywhere real).

## Configuration

Edit `vmcp.toml` (see inline comments). Every key is overridable via env
vars with the `VMCP_` prefix and `__` as nested separator, e.g.

```bash
VMCP_AUTH__MASTER_PASSWORD_ARGON2='$argon2id$...' cargo run -p vmcp
```

Generate a password hash:

```bash
cargo run -p vmcp -- hash-password --password 'your-secret'
```

Print the resolved config and exit:

```bash
cargo run -p vmcp -- print-config
```

## Workspace layout

| Crate            | Purpose                                                                  |
| ---------------- | ------------------------------------------------------------------------ |
| `vmcp`           | Entry binary. Wires axum + rmcp + every library crate.                   |
| `vmcp-config`    | Config loading (figment + TOML + env override).                          |
| `vmcp-registry`  | `registry.json`, sidecar specs, `tools.lock.json`.                       |
| `vmcp-upstream`  | Upstream pool — stdio child-process MCP clients via rmcp.                |
| `vmcp-graphql`   | Dynamic GraphQL schema generation from upstream `tools/list`.            |
| `vmcp-auth`      | OAuth 2.1 + PKCE + DCR, argon2id, JWKS rotation, `require_bearer`.       |
| `vmcp-server`    | MCP surface (`query_graphql` tool), skills loader, proxy endpoint.       |
| `vmcp-notify`    | In-process notification ring buffer (tokio broadcast).                   |
| `vmcp-admin`     | Admin UI + recording/playback.                                           |

## License

MIT — see [LICENSE](LICENSE).
