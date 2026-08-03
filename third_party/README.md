# Vendored dependencies

## `rmcp` (1.7.0 + session resume fix)

Upstream: [`modelcontextprotocol/rust-sdk`](https://github.com/modelcontextprotocol/rust-sdk) crate `rmcp` **1.7.0**.

### Why vendored

Streamable HTTP request-wise channels were removed from `tx_router` as soon as a
JSON-RPC **response or error** was written. That discarded the completed-event
cache before `unregister_resource` could mark the channel completed, so a later
`GET /mcp` with `Last-Event-ID` failed with:

```text
Resume failed (Session error: Channel closed: Some(N)), returning empty stream
```

EventSource clients reconnect after the SSE priming `retry: 3000` (~3s). An empty
resume stream looks like a dead session and triggers re-`initialize` storms
(see GitHub issue [#2](https://github.com/hewimetall/vmcp/issues/2)).

### Local change

In `src/transport/streamable_http_server/session/local.rs` (`handle_server_message`):
do **not** `tx_router.remove` when `close: true`. Keep the entry so
`unregister_resource` can close the sender, set `completed_at`, and retain the
cache for resume (same path for success and error).

Wired via root `Cargo.toml`:

```toml
[patch.crates-io]
rmcp = { path = "third_party/rmcp" }
```

Drop this patch once an upstream `rmcp` release includes the same fix.
