# Security Policy

## Supported Versions

vmcp is pre-1.0. Only the latest `0.2.x` line receives security fixes.

| Version | Supported |
| ------- | --------- |
| 0.2.x   | yes       |
| < 0.2   | no        |

## Reporting a Vulnerability

Please **do not** open public GitHub issues for security problems.

Use the private channel:

- GitHub Security Advisories: https://github.com/hewimetall/vmcp/security/advisories/new

We aim to acknowledge new reports within 7 days. If a report is accepted,
we will coordinate a fix and disclosure timeline with you before publishing
the advisory.

## Scope

In scope:

- The `vmcp` binary and all crates in this workspace (`crates/vmcp-*`).
- Authentication and authorization (OAuth 2.1, JWT, argon2id password
  handling) implemented in `vmcp-auth` and `vmcp-server`.
- GraphQL request handling and schema generation in `vmcp-graphql`.
- Upstream lifecycle and process isolation in `vmcp-upstream`.

Out of scope:

- Third-party MCP servers spawned as upstreams. Report those to their
  respective maintainers.
- Misconfiguration of an operator's own deployment (e.g. running vmcp with
  the default `demo-master` password in production).
- Vulnerabilities in third-party crates listed in `Cargo.lock` — those
  should be reported upstream; we will pick up fixed versions on release.
