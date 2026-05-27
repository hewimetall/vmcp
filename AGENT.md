# AGENT.md

Guidance for coding agents working in this repository.

## Project

vmcp is a Rust MCP gateway. Cargo workspace, 9 crates, entry binary
`vmcp`. Speaks the Model Context Protocol over streamable HTTP and
exposes upstream MCP servers as one GraphQL schema.

## Setup

```bash
cargo build --workspace
```

Rust 1.80+. No system dependencies for `cargo build` / `cargo run`.

For the default demo registry you also need Node on `$PATH` (for `npx`).

## Run

```bash
cargo run -p vmcp
```

Listens on `http://127.0.0.1:8765`. MCP surface at `/mcp` (bearer-auth),
`/health` is open. Override config path with `--config` or
`VMCP_CONFIG=`.

Subcommands:

- `cargo run -p vmcp -- hash-password --password '...'` — argon2id hash.
- `cargo run -p vmcp -- print-config` — dump the resolved config.

## Test

```bash
cargo test --workspace
```

Single crate: `cargo test -p vmcp-graphql`.

## Lint and format

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
```

Both must pass clean before opening a PR.

## Code style

- Rust 2021, MSRV 1.80 (`workspace.rust-version` in `Cargo.toml`).
- rustfmt defaults — do not configure overrides.
- Errors: `anyhow::Result` at binary and external boundaries, `thiserror`
  for crate-internal error types (workspace deps already pin both).
- No `unwrap` / `expect` outside of `main.rs` boot and tests.
- Tracing via `tracing` macros; no `println!` outside of CLI output.

## Architecture pointers

Where to look first when changing X:

| Concern                              | Crate              |
| ------------------------------------ | ------------------ |
| Lazy discovery ladder, schema build  | `vmcp-graphql`     |
| OAuth 2.1, PKCE, JWKS, argon2id      | `vmcp-auth`        |
| Upstream lifecycle, child-process    | `vmcp-upstream`    |
| Hot-swap registry, sidecar specs     | `vmcp-registry`    |
| `query_graphql` MCP tool, skills     | `vmcp-server`      |
| Config loading, env overrides        | `vmcp-config`      |
| Notification ring buffer             | `vmcp-notify`      |
| Admin UI, recorder, playback         | `vmcp-admin`       |
| Wiring everything together           | `vmcp` (`main.rs`) |

The boot path in `crates/vmcp/src/main.rs` is the canonical reading
order: config → notify bus → upstream pool → tools lock → schema build →
`VmcpServer` → axum router with `require_bearer` on `/mcp`.

## PR expectations

- `cargo fmt --all`, `cargo clippy --workspace --all-targets`, and
  `cargo test --workspace` must pass.
- Keep crate boundaries. Don't pull `vmcp-graphql` types into
  `vmcp-server` or vice versa — go through the public API.
- New upstream-facing behavior usually lands in `vmcp-upstream` or
  `vmcp-graphql`; new HTTP surface lands in `vmcp-server` or
  `vmcp-auth`.
- One logical change per PR. Avoid renames or formatting churn mixed
  with substantive changes.
