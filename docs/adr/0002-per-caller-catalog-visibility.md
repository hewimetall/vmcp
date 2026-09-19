# ADR 0002 — Per-caller catalog visibility (G25)

**Status:** Accepted  
**Date:** 2026-08-19  
**Context:** [hewimetall/vmcp#7](https://github.com/hewimetall/vmcp/issues/7) — multi-tenant gateway (vmcp 1.2.0 + Authentik groups). Call grants already used `upstream:<name>` whitelist; discovery still leaked every tenant namespace.

## Problem

One gateway served many isolated stands. Identity and call enforcement worked:

- Authentik group → `group_scopes` → `upstream:<stand>`
- `ScopePolicy::authorize` blocked `tools/call` / GraphQL resolvers / `run_task` for other stands

The **catalogue was global**. A tenant who called GraphQL `{ servers { name } }`, `search(q)`, `__type(name: "Query"|"Mutation")`, or `/mcp-proxy` `tools/list` learned every other stand's upstream name. They could not *call* those tools, but names leaked.

That blocked the topology the operators wanted: **one gateway per tenant, federated via `peerGatewayRef`**. Structural isolation only helps if the top-level catalog does not list every peer by name.

A second, optional ask (lazy connect with caller identity on handshake) is **out of scope** here — see Consequences.

## Decision

Reuse the request-scoped `ScopePolicy` already attached to `/mcp` and `/mcp-proxy`. Do **not** invent a second ACL.

| Caller | Catalogue |
| ------ | --------- |
| At least one `upstream:<name>` and **no** `mcp:admin` (whitelist mode) | Only those upstreams |
| `mcp:admin` (even if `upstream:*` tokens are also present) | Full catalogue |
| No `upstream:*` tokens (`mcp:use` / `mcp:read` / `mcp:write`) | Full catalogue — same as call grants |

Surfaces that must honor the same predicate:

- GraphQL `servers`, `search`, `prompts`, `searchPrompts`, `getPrompt`
- GraphQL `notifications` (`source` is an upstream name)
- GraphQL Query/Mutation **namespace fields** (resolver + scoped schema so `__type` / introspection do not list foreign fields)
- `/mcp-proxy` `tools/list` and `prompts/list` (`prompts/get` for a hidden upstream → unknown)

`mcp:use` without `upstream:*` is **not** tenant isolation. Those tokens may still call every namespace; hiding names would be theatre.

Call enforcement is **unchanged**: `authorize` still applies the whitelist even for `mcp:admin` if `upstream:*` tokens are present. Admin's exception is discovery-only, so operators can inspect the full federated graph without widening tool grants.

### Why a scoped GraphQL schema (not only resolver filters)

Dynamic `async-graphql` fields have no `visible` hook. Filtering `servers` / `search` alone still leaves `__type(name: "Mutation") { fields { name } }` listing every tenant — the discovery ladder in `clients.md` tells agents to introspect Query/Mutation. For whitelist callers, `query_graphql` rebuilds a schema from the allowed `pool.all_resolved()` subset. Resolver filters remain as defence-in-depth (pool snapshot is still global).

## Consequences

- **Breaking for clients that assumed a global catalog** while holding `upstream:*` scopes: they stop seeing other namespaces. That is the point of #7.
- Federated per-tenant gateways can advertise `peerGatewayRef` names only to callers who are scoped to them (or to `mcp:admin`).
- Local YAML skills stay visible (no `server`); upstream `{server}__{prompt}` names follow the same whitelist.
- Admin UI / `schema_swap` keep the full schema (operator surface).
- **Not decided:** lazy upstream connect + `tools/list` with caller identity on first authorized use (#7 Ask 2). Boot still handshakes every enabled upstream anonymously. Catalog isolation does not require that change.

## Example

```toml
[auth]
enabled = true
provider = "authentik"

[auth.authentik]
group_scopes = { "dayana" = "mcp:use upstream:dayana", "admin" = "mcp:admin" }
```

Token `mcp:use upstream:dayana` → `{ servers { name } }` is `[dayana]`.  
Token `mcp:admin` → full `{ servers { name } }`.

## Verification

Same ladder as [clients.md](../clients.md) (`prompts` → `servers` → `search`/`searchPrompts` → `__type`), plus `/mcp-proxy` `tools/list` from [upstreams.md](../upstreams.md) and local YAML from [skills.md](../skills.md):

```bash
cargo test -p vmcp --test catalog_g25
```
