VMCP_PATCHED=1
UPSTREAM_VERSION=3.1.0
ISSUE=https://github.com/hewimetall/vmcp/issues/2

Patched `handle_server_message` in
`src/transport/streamable_http_server/session/local.rs`:
do not remove completed request-wise channels from `tx_router` so
`unregister_resource` can retain the SSE event cache for Last-Event-ID resume.
