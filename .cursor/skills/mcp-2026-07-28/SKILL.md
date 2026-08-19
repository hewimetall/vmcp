---
name: mcp-2026-07-28
description: MUST use MCP protocol 2026-07-28 when creating or migrating MCP servers, clients, or this vmcp gateway. Use when the user asks to build an MCP server, wrap an API for MCP, or upgrade vmcp/rmcp to the 2026-07-28 spec.
---

# MCP 2026-07-28 (MUST)

This overlay **overrides** the vendored Anthropic plugin at
`third_party/mcp-server-dev/`. That plugin is what
https://modelcontextprotocol.io/docs/2026-07-28/develop/build-with-agent-skills
points to, but its internals still describe **2025-11-25**.

**Protocol version string:** `2026-07-28`  
**Spec:** https://modelcontextprotocol.io/specification/2026-07-28  
**Changelog vs 2025-11-25:** https://modelcontextprotocol.io/specification/2026-07-28/changelog  
**Rust SDK in this repo:** vendored `rmcp` 3.1.0 (`third_party/rmcp`) already
knows `ProtocolVersion::V_2026_07_28`, but `ProtocolVersion::LATEST` is still
`2025-11-25`.

Do not scaffold `initialize` + `notifications/initialized` as the modern
lifecycle. Do not tell agents to send server-initiated `roots/list`,
`sampling/createMessage`, or `elicitation/create` on 2026-07-28 sessions.

## MUST / MUST NOT (modern path)

| Topic | 2025-11-25 (legacy) | 2026-07-28 MUST |
| ----- | ------------------- | --------------- |
| Handshake | `initialize` + `initialized` | Optional `server/discover`; every RPC is self-contained |
| Version | `params.protocolVersion` once | `_meta["io.modelcontextprotocol/protocolVersion"]` on **every** request |
| Client identity | `clientInfo` at initialize | `_meta["io.modelcontextprotocol/clientInfo"]` + `clientCapabilities` per request |
| Server identity | `initialize` result | `_meta["io.modelcontextprotocol/serverInfo"]` on results |
| HTTP sessions | `Mcp-Session-Id` | **Removed.** Cross-call state = explicit handles in tool args (SEP-2567) |
| SSE resume | `Last-Event-ID` | **Removed.** Broken stream → new request id (SEP-2575) |
| GET `/mcp` | standalone SSE | Replaced by `subscriptions/listen` POST |
| Subscribe | `resources/subscribe` | `subscriptions/listen` opt-in types + `subscriptionId` |
| Ping / setLevel | `ping`, `logging/setLevel` | **Removed.** Log level = `_meta["io.modelcontextprotocol/logLevel"]` |
| Server→client RPC | server sends requests on SSE | **MRTR:** `resultType: "input_required"` + retry with `inputResponses` (SEP-2322) |
| Results | no discriminator | `resultType: "complete"` required (legacy omit = complete) |
| HTTP POST headers | optional | `Mcp-Method` + `Mcp-Name` required (SEP-2243) |
| List/read cache | none | `ttlMs` + `cacheScope` on list/read results (SEP-2549) |
| Tasks | core SEP-1686 (`tasks/result`, `tasks/list`) | Extension `io.modelcontextprotocol/tasks`; poll `tasks/get`; `tasks/update`; no `tasks/list` (SEP-2663) |
| Roots / sampling / logging | active | **Deprecated** (SEP-2577). Do not add new uses |
| DCR | common | **Deprecated.** Prefer CIMD (SEP-991). Keep DCR only as fallback |
| Resource not found | `-32002` | `-32602` |
| Header mismatch | `-32001` | `-32020` |

## Creating a new MCP server (2026-07-28)

Still use the vendored plugin for **product** questions (remote HTTP vs MCPB,
tool-design patterns, CIMD). Then apply this overlay before writing code:

1. Transport: Streamable HTTP. Stateless. No session id.
2. Implement `server/discover` (`supportedVersions` includes `"2026-07-28"`).
3. Every request `_meta` must carry protocol version + client capabilities.
4. Every result includes `resultType`. List endpoints include `ttlMs`/`cacheScope`.
5. Need user input mid-tool → MRTR `InputRequiredResult`, not `elicitInput()`.
6. Long work → tasks **extension**, poll `tasks/get`. Do not implement `tasks/result`.
7. Auth: CIMD first; DCR only if the host cannot do CIMD.
8. Language for *this* repo: Rust + `rmcp`, not the TS/Python scaffolds in the plugin.

Scaffolds live in:

- `third_party/mcp-server-dev/skills/build-mcp-server/references/remote-http-scaffold.md`
- `third_party/mcp-server-dev/skills/build-mcp-server/references/auth.md` (treat CIMD as required, DCR as fallback)

## Migrating vmcp (this gateway)

vmcp is **both** an MCP server (`/mcp`, `/mcp-proxy`) **and** an MCP client
(upstream pool). Dual-protocol is mandatory: keep 2025-11-25 for existing
hosts (Cursor, tests) while serving 2026-07-28 for modern clients.

See `docs/mcp-2026-07-28.md` for the complexity assessment and work breakdown.

Do not delete `LocalSessionManager` or Last-Event-ID resume until the
legacy path is explicitly retired. `rmcp` already dual-stacks when
`legacy_session_mode` is true (default).
