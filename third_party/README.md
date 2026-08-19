# Vendored dependencies

## `rmcp` (3.1.0 + session resume fix)

Upstream: [`modelcontextprotocol/rust-sdk`](https://github.com/modelcontextprotocol/rust-sdk) crate `rmcp` **3.1.0**.

### Why vendored

Streamable HTTP request-wise channels were removed from `tx_router` as soon as a
JSON-RPC **response or error** was written. That discarded the completed-event
cache before `unregister_resource` could mark the channel completed, so a later
`GET /mcp` with `Last-Event-ID` failed with:

```text
Resume failed (Session error: Channel closed: Some(N)), returning empty stream
```

Still present on crates.io `rmcp` 3.1.0 and upstream `main` (2026-08-03).
EventSource clients reconnect after SSE priming `retry: 3000` and treat the
empty resume as a dead session (see [#2](https://github.com/hewimetall/vmcp/issues/2)).

### Local change

In `src/transport/streamable_http_server/session/local.rs` (`handle_server_message`):
do **not** `tx_router.remove` when `close: true`. Keep the entry so
`unregister_resource` can close the sender, set `completed_at`, and retain the
cache for resume.

```toml
[patch.crates-io]
rmcp = { path = "third_party/rmcp" }
```

Drop this patch once an upstream `rmcp` release includes the same fix.

## `mcp-server-dev` (Anthropic skills snapshot)

Official MCP builder skills linked from the **2026-07-28** docs
([Build with Agent Skills](https://modelcontextprotocol.io/docs/2026-07-28/develop/build-with-agent-skills)).

Snapshot + spec-pin gap: [`mcp-server-dev/SOURCE.md`](mcp-server-dev/SOURCE.md).

The plugin still documents protocol **2025-11-25**. For this repo, protocol
**MUST** be **2026-07-28** — see
[`.cursor/skills/mcp-2026-07-28/SKILL.md`](../.cursor/skills/mcp-2026-07-28/SKILL.md)
and [`docs/mcp-2026-07-28.md`](../docs/mcp-2026-07-28.md).
