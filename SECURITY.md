# Security Policy

**Language:** English | [Русский](SECURITY.ru.md)

## Supported versions

Only the latest published release series on GitHub Releases (**1.0.x**) receives
security fixes. The `version` field in the workspace `Cargo.toml` must match the
`v1.0.x` tag.

| Version | Supported |
| ------- | --------- |
| 1.0.x | Yes |

## Reporting a vulnerability

Please **do not** open public GitHub issues for security vulnerabilities.

Use this private reporting channel:

- GitHub Security Advisories: https://github.com/hewimetall/vmcp/security/advisories/new

We aim to acknowledge new reports within 7 days. If we accept a report, we will
coordinate the fix and disclosure timeline with you before publishing an
advisory.

## Scope

In scope:

- The `vmcp` binary and all crates in this workspace (`crates/vmcp-*`).
- Authentication and authorization implemented in `vmcp-auth` and
  `vmcp-server`, including OAuth 2.1, JWT, and argon2id password handling.
- GraphQL request processing and schema generation in `vmcp-graphql`.
- Upstream server lifecycle management and process isolation in
  `vmcp-upstream`.

Out of scope:

- Third-party MCP servers run as upstreams. Report these issues to their
  maintainers.
- Misconfiguration of an operator-managed deployment, such as running vmcp in
  production with the default `demo-master` password or unnecessarily setting
  `[proxy] enabled = true` on a public origin. These are demo defaults in
  `vmcp.toml`; change them before deploying to production.
- Vulnerabilities in third-party crates listed in `Cargo.lock`. Report these to
  the upstream projects; we will adopt patched versions in a release.
