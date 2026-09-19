# How to connect OpenCode to vmcp

**Language:** English | [Русский](../ru/how-to/connect-opencode.md)

Connect OpenCode to a remote vmcp gateway and complete OAuth authorization from
the OpenCode CLI.

## Prerequisites

- A vmcp gateway available at a public HTTPS URL
- `auth.enabled = true` on the gateway
- The vmcp master password
- OpenCode installed locally
- If `auth.dcr_redirect_uri_allowlist` is restricted, allow OpenCode's loopback
  callback by including `http://127.0.0.1` and `http://localhost`

## Steps

### 1. Add vmcp to the OpenCode configuration

Create or update `opencode.json` in your project:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "vmcp": {
      "type": "remote",
      "url": "https://gateway.example.com/mcp",
      "enabled": true,
      "timeout": 30000
    }
  }
}
```

Replace `gateway.example.com` with the public vmcp host. The URL must end in
`/mcp`.

### 2. Start OAuth authorization

Run:

```bash
opencode mcp auth vmcp
```

OpenCode discovers vmcp's OAuth endpoints, performs Dynamic Client Registration
(DCR) with Proof Key for Code Exchange (PKCE), and opens the consent page in
your browser.

### 3. Approve the connection

Enter the vmcp master password on the `/consent` page. Enter the plain-text
password, not its Argon2 hash.

OpenCode stores the resulting credentials in
`~/.local/share/opencode/mcp-auth.json`.

### 4. Check the connection

Run:

```bash
opencode mcp list
```

The `vmcp` entry should report a connected or authenticated state.

### 5. Call vmcp from OpenCode

Start OpenCode and send:

```text
Use vmcp. Call query_graphql once with:
{ servers { name description toolCount readOnlyCount } }
Return the result as a table.
```

OpenCode should use vmcp's `query_graphql` tool and list the connected upstream
servers.

## Verify it worked

- `opencode mcp list` shows **vmcp** as connected.
- The agent exposes a vmcp-prefixed `query_graphql` tool.
- The test prompt returns the upstream names from `registry.json`.
- The OpenCode client appears in the vmcp **Sessions** page after its first MCP
  request.

## Use a static token instead

If you do not want OpenCode to start OAuth, create a static token as described
in [Authentication](../authentication.md#static-bearer-tokens-pre-reg), export
it as `VMCP_TOKEN`, and set `oauth` to `false`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "vmcp": {
      "type": "remote",
      "url": "https://gateway.example.com/mcp",
      "enabled": true,
      "oauth": false,
      "headers": {
        "Authorization": "Bearer {env:VMCP_TOKEN}"
      }
    }
  }
}
```

## Troubleshooting

| Problem | Fix |
| ------- | --- |
| Authorization does not start | Run `opencode mcp auth vmcp` explicitly. |
| The callback is rejected | Allow the loopback callback in `auth.dcr_redirect_uri_allowlist`, or leave the allowlist empty. |
| OpenCode reports an OAuth discovery error | Run `opencode mcp debug vmcp` and verify `public_base_url` matches the public HTTPS origin. |
| A static token returns 401 | Confirm the token is present in `auth.tokens_file` and that `VMCP_TOKEN` is exported in the OpenCode process environment. |
| Tool discovery times out | Keep `timeout` at `30000` or increase it when upstream startup is slow. |

## Next steps

- Follow the [`query_graphql` discovery ladder](../clients.md#using-the-graphql-tool).
- Limit the client with [vmcp scopes](../authentication.md#scopes-enforced).
- Read the official [OpenCode MCP documentation](https://opencode.ai/docs/mcp-servers/).
