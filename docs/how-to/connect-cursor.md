# How to connect Cursor to vmcp

**Language:** English | [Русский](../ru/how-to/connect-cursor.md)

Connect Cursor to a deployed vmcp gateway over Streamable HTTP and authorize it
with vmcp's OAuth flow.

## Prerequisites

- A vmcp gateway available at a public HTTPS URL
- `auth.enabled = true` on the gateway
- The vmcp master password
- Cursor with MCP support
- If `auth.dcr_redirect_uri_allowlist` is restricted, the Cursor callback you
  use must be allowed:
  - Desktop: `http://localhost:8787/callback`
  - Web and Cursor Agents: `https://www.cursor.com/agents/mcp/oauth/callback`

## Steps

### 1. Create the Cursor MCP configuration

Create `.cursor/mcp.json` in a project, or `~/.cursor/mcp.json` for a global
configuration:

```json
{
  "mcpServers": {
    "vmcp": {
      "url": "https://gateway.example.com/mcp"
    }
  }
}
```

Replace `gateway.example.com` with the public vmcp host. The URL must end in
`/mcp`.

### 2. Enable the server in Cursor

Restart Cursor, open **Customize > MCPs**, and enable **vmcp**. Cursor connects
to the endpoint, discovers OAuth metadata, and opens the authorization flow in
your browser.

### 3. Authorize Cursor

Enter the vmcp master password on the `/consent` page. Enter the plain-text
password, not its Argon2 hash.

After approval, Cursor stores the OAuth client and access token. vmcp persists
the DCR `client_id` in `auth.clients_db_path`; the access token may need to be
refreshed after a gateway restart.

### 4. Call vmcp from chat

Send this prompt in Cursor:

```text
Use the vmcp query_graphql tool once with this document:
{ servers { name description toolCount readOnlyCount } }
Return the result as a table.
```

Cursor should call `query_graphql` once and list the upstream servers currently
connected to vmcp.

## Verify it worked

- **vmcp** appears as connected under **Customize > MCPs**.
- Cursor lists `query_graphql` under the server's available tools.
- The test prompt returns the names from your `registry.json`.
- The Cursor client appears in the vmcp **Sessions** page after its first MCP
  request.

## Use a static token instead

If browser-based OAuth is unavailable, create a static token on the gateway
host and configure `auth.tokens_file` as described in
[Authentication](../authentication.md#static-bearer-tokens-pre-reg). Export
the token before starting Cursor:

```bash
export VMCP_TOKEN='vmcp_...'
```

Then add an authorization header:

```json
{
  "mcpServers": {
    "vmcp": {
      "url": "https://gateway.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${env:VMCP_TOKEN}"
      }
    }
  }
}
```

Do not commit the token. Project-level `.cursor/mcp.json` files may be shared
with the repository.

## Troubleshooting

| Problem | Fix |
| ------- | --- |
| Cursor cannot reach vmcp | Open `https://gateway.example.com/health` and confirm it returns `ok`. |
| OAuth opens on the wrong host | Make `public_base_url` and `auth.issuer` match the public HTTPS origin. |
| DCR rejects the callback | Add the active Cursor callback URL to `auth.dcr_redirect_uri_allowlist`, or leave the allowlist empty. |
| A static token returns 401 | Confirm `auth.tokens_file` points to the file that contains the token and restart Cursor with `VMCP_TOKEN` in its environment. |
| The server connects but no tool appears | Open **Output**, select **MCP Logs**, and confirm the configured URL ends in `/mcp`. |

## Next steps

- Follow the [`query_graphql` discovery ladder](../clients.md#using-the-graphql-tool).
- Configure [scopes and per-upstream access](../authentication.md#scopes-enforced).
- Read the official [Cursor MCP documentation](https://cursor.com/docs/mcp).
