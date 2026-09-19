# vmcp Documentation

**Language:** English | [Русский](ru/README.md)

Operator guide for deploying and operating the virtual MCP gateway.

| Document | Description |
| -------- | ----------- |
| [deployment.md](deployment.md) | Deployment from GHCR: Compose + Caddy, bare metal, TLS, environment variables, and checklist |
| [authentication.md](authentication.md) | OAuth 2.1 flow, master password, static tokens, Authentik hop trust, and auth-free development mode |
| [adr/0001-forward-auth-trust-and-identity-propagation.md](adr/0001-forward-auth-trust-and-identity-propagation.md) | ADR: trust for `X-authentik-*` headers and `X-Vmcp-*` identity on HTTP upstreams |
| [adr/0002-per-caller-catalog-visibility.md](adr/0002-per-caller-catalog-visibility.md) | ADR: catalog filtering by the `upstream:<name>` whitelist (G25), not just call filtering |
| [builds-and-modes.md](builds-and-modes.md) | Cargo features, release binaries, the HTTP gateway, and optional `[tasks]` |
| [upstreams.md](upstreams.md) | Registering upstream services, tools (sidecar + lockfile), and prompts |
| [tasks.md](tasks.md) | Native MCP Tasks (`run_task`), SQLite store, allowlist, and the SEP-1686 flow |
| [sessions.md](sessions.md) | Registry of admin sessions and recordings (JSON in `sessions_dir`) |
| [skills.md](skills.md) | YAML skill playbooks exposed through MCP `prompts/list` / `prompts/get` |
| [clients.md](clients.md) | Cursor over HTTP + OAuth, scripted MCP/HTTP clients, and vmcp-lite for local stdio hosts |
| [bench.md](bench.md) | Optional Python tool for measuring how LLMs batch `query_graphql` requests |
| [aggregation.md](aggregation.md) | How GraphQL aggregation works across upstream tools |
| [mcp-2026-07-28.md](mcp-2026-07-28.md) | Evaluation of the 2026-07-28 spec, session remapping, the `[mcp].latest` flag, and gateway patterns |

Quick links from the repository root:

- Configuration template: [`vmcp.toml`](../vmcp.toml)
- Docker stack: [`deploy/bootstrap.sh`](../deploy/bootstrap.sh) + [`docker-compose.yml`](../docker-compose.yml) + [`deploy/Caddyfile`](../deploy/Caddyfile) (image from GHCR / `release` workflow)
- Demo: [`demo/README.md`](../demo/README.md) + [`demo/vmcp.toml`](../demo/vmcp.toml)
