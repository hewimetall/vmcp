# Demo

**Language:** English | [Русский](README.ru.md)

A small test environment comprising the gateway and several MCP upstreams that
operate on the [`stand/`](stand/) project. Configuration:
[`vmcp.toml`](vmcp.toml).

## What's included

| Upstream | Purpose |
|----------|---------|
| `time` | Time and time zones |
| `filesystem` | Files in `stand/` |
| `architect_c4` | C4 models in `stand/docs/` |
| `agent_lsp` | Python LSP support for `stand/` |
| `context7` | Library documentation (requires `CONTEXT7_API_KEY`) |

## Prerequisites

- the `vmcp` binary (from a GitHub release) **or** `cargo run -p vmcp`
- `uv` (for `uvx`)
- Node.js / `npx`
- `agent-lsp` + language servers:
  - demo Python: `pip install agent-lsp` + `npm i -g pyright`
  - this Rust workspace: `pip install agent-lsp` + `rustup component add rust-analyzer`
    then `./scripts/run-agent-lsp.sh` → MCP HTTP on `http://127.0.0.1:8766`

## Run the demo

From the repository root:

```bash
# demo/registry.json interpolates ${CONTEXT7_API_KEY}; unset aborts boot.
# Empty skips the Context7 Authorization header. Set a real key to use that upstream.
export CONTEXT7_API_KEY="${CONTEXT7_API_KEY:-}"

./vmcp --config ./demo/vmcp.toml
# or:
cargo run -p vmcp -- --config ./demo/vmcp.toml
```

Health check:

```bash
curl -fsS http://127.0.0.1:8765/health
# ok
```

Automated smoke test:

```bash
VMCP_BIN=./vmcp python3 demo/smoke_demo_gateway.py
```

## Example calls

Authentication is disabled in `demo/vmcp.toml`. The MCP endpoint is
`http://127.0.0.1:8765/mcp`; use the `query_graphql` tool.

List servers:

```graphql
{ servers { name toolCount } }
```

Get the current time:

```graphql
{
  time {
    getCurrentTime(timezone: "Europe/Moscow") { json }
  }
}
```

Read a file from the test project:

```graphql
{
  filesystem {
    readTextFile(path: "src/main.py") { json text }
  }
}
```

C4 model:

```graphql
{
  architectC4 {
    getModel { json }
  }
}
```

LSP (start by passing the absolute path to the project root):

```graphql
mutation {
  agentLsp {
    startLsp(rootDir: "/ABS/PATH/TO/new_vmcp/demo/stand") { json }
  }
}
```

```graphql
{
  agentLsp {
    listSymbols(filePath: "/ABS/PATH/TO/new_vmcp/demo/stand/src/main.py") { json }
  }
}
```

Context7 (if the API key is set):

```graphql
{
  context7 {
    resolveLibraryId(libraryName: "react") { json }
  }
}
```

If an upstream fails to start, the gateway continues to run; that upstream is
simply omitted from `servers`.
