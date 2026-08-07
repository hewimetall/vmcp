# ADR 0001 — Forward-auth hop trust & caller identity to upstreams

**Status:** Accepted  
**Date:** 2026-08-07  
**Context:** mcpwork.space operators (vmcp 1.1.0) reported two product-contract gaps blocking multi-tenant adapters.

## Problem

1. With `auth.authentik.forward_auth = true`, any TCP client that could reach `/mcp` could forge `X-authentik-username` / `X-authentik-groups` and mint an MCP session (including `mcp:admin` via mapped groups).
2. After authenticating the caller, vmcp invoked HTTP upstreams with only a static registry `bearer`. Adapters could not authorize `stand_slug` (or similar) against the real subject/groups.

## Decision

### Defect 1 — Hop trust for forward-auth

When `forward_auth = true`, operators **must** configure at least one of:

| Knob | Meaning |
| ---- | ------- |
| `trusted_proxies = ["10.244.0.0/16", …]` | TCP peer (`ConnectInfo`) must match a CIDR |
| `forward_auth_secret` (+ optional `forward_auth_secret_header`, default `x-vmcp-forward-auth`) | Gateway injects a shared secret; constant-time compare |

If both are set, **both** must pass (AND). If neither is set, config load / `AuthentikAuth::new` fails closed.

Bearer JWT path is unchanged (signature + audience verify). The same hop trust gates `/admin` when `auth.admin.mode = authentik`.

### Defect 2 — Identity forwarding on HTTP upstream calls (opt-in)

Operators mix **external** SaaS MCP servers and **internal** adapters. Identity
headers must not leak to the public internet by default.

`forward_identity` is **per-upstream**, default **`false`**:

- External (Notion, Context7, …): leave off.
- Internal (`stand-api-mcp`, …): set `"forward_identity": true` or
  `vmcp add mcp --forward-identity …`.

When enabled, on every `tools/call` to that HTTP upstream:

| Header | Value |
| ------ | ----- |
| `Authorization` | Registry `bearer` only (service credential; unchanged) |
| `X-Vmcp-Subject` | Authenticated subject |
| `X-Vmcp-Groups` | Comma-separated groups |
| `X-Vmcp-Client-Id` | Client / username |
| `X-Vmcp-Scope` | Resolved MCP scopes |

vmcp asserts these headers **after** its own auth (JWT / trusted hop) — they
are not client-forged ingress headers (that is Defect 1). Identity is stored in
a per-session slot under `call_lock` and injected by an identity-aware
Streamable HTTP client.

Stdio upstreams do not receive HTTP headers (no change).

## Consequences

- **Breaking:** Existing Authentik forward-auth deployments must set `trusted_proxies` and/or `forward_auth_secret` before upgrade.
- Adapters (e.g. `stand-api-mcp`) can authorize tenancy from `X-Vmcp-*` without embedding secrets in tool args.
- Edge stripping of inbound `X-authentik-*` remains a good defence-in-depth; the application no longer trusts those headers from arbitrary peers.

## Example (Gateway + Authentik)

```toml
[auth]
enabled = true
provider = "authentik"

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
accept_bearer = true
forward_auth = true
trusted_proxies = ["10.244.0.0/16"]          # Gateway / mesh CIDR
# forward_auth_secret via VMCP_AUTH__AUTHENTIK__FORWARD_AUTH_SECRET
group_scopes = { "mcp-users" = "mcp:use", "admin" = "mcp:admin" }
```

Gateway `HTTPRoute`: strip client `X-authentik-*` and `X-Vmcp-Forward-Auth`, then let Authentik / the outpost re-inject identity headers and the hop secret only on the trusted hop.
