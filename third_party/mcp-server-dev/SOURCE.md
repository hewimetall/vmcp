# mcp-server-dev (vendored)

Official MCP development skills linked from
[Build with Agent Skills](https://modelcontextprotocol.io/docs/2026-07-28/develop/build-with-agent-skills)
(protocol docs era **2026-07-28**).

| Field | Value |
| ----- | ----- |
| Upstream | https://github.com/anthropics/claude-plugins-official |
| Path | `plugins/mcp-server-dev` |
| Snapshot | `ff2a7b576c20b2f1a777c497455e5d1e443f23e9` (2026-08-19) |
| License | Apache-2.0 (see `LICENSE`) |

## Spec pin gap

The docs page lives under `/docs/2026-07-28/`, but this plugin’s own
`skills/build-mcp-server/references/versions.md` still pins **MCP spec
2025-11-25** (last verified 2026-03). Scaffolding and elicitation examples
assume `initialize`, `Mcp-Session-Id`, server-initiated `elicitInput`,
Roots/Sampling/Logging.

For **this repository**, treat
[`.cursor/skills/mcp-2026-07-28/SKILL.md`](../../.cursor/skills/mcp-2026-07-28/SKILL.md)
as the MUST overlay: protocol **2026-07-28** wins over the vendored plugin.

Do not “fix” the upstream copies in place; re-snapshot from
`anthropics/claude-plugins-official` when they catch up.
