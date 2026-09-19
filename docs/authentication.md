# Authentication

**Language:** English | [Русский](ru/authentication.md)

`/mcp` (and `/mcp-proxy`) are protected by an **authentication facade**: either the built-in OAuth 2.1 AS/RS (`provider = "local"`) or external [Authentik](https://github.com/goauthentik/authentik) (`provider = "authentik"`). `/admin` uses a separate facade with three modes.

## Surfaces

| Path | Authentication | Purpose |
| ---- | -------------- | ------- |
| `/mcp` | Bearer JWT / `vmcp_…` / Authentik forward auth | MCP Streamable HTTP |
| `/mcp-proxy` | same (when `[proxy]` is enabled) | Transparent upstream tools |
| `/admin` | `none` \| HTTP Basic \| Authentik headers | Operator SPA |
| `/health` | none | Liveness |
| `/ready` | none | Readiness (soft: ≥1 connected upstream when the registry contains an enabled upstream) |
| `/authorize`, `/consent`, `/token`, `/register`, `/.well-known/*` | none | OAuth + metadata (local AS) |

---

## Authentication facade: `local` vs `authentik`

Every protected request passes through a single entry point (`AuthFacade`). An anonymous or default role is **never** substituted.

| | `provider = "local"` (default) | `provider = "authentik"` |
| - | ------------------------------ | --------------------------------- |
| Token issuer | vmcp (DCR + PKCE + consent) | Authentik OAuth2/OIDC |
| MCP client | Bearer JWT or `vmcp_…` | Bearer JWT from Authentik |
| Browser behind the gateway | — | `X-authentik-username` + `X-authentik-groups` |
| JWT verification | local JWKS | remote JWKS (rustls `reqwest`) + Authentik JWKS |
| Local `/authorize`… | yes | no (PRM points to Authentik only) |

### Authentik (recommended setup without DCR)

```toml
[auth]
enabled = true
provider = "authentik"
# master_password_argon2 is optional; set it only if /admin Basic is needed

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
# empty → public_base_url + /mcp (+ /mcp-proxy)
audiences = ["https://architecture.mcpwork.space/mcp"]
accept_bearer = true          # MCP clients
forward_auth = true           # browser behind Envoy/Caddy forward auth
# REQUIRED with forward_auth: hop trust (otherwise any peer can send X-authentik-*)
trusted_proxies = ["10.244.0.0/16"]
# alternatively/in addition: forward_auth_secret = "…"  (env: VMCP_AUTH__AUTHENTIK__FORWARD_AUTH_SECRET)
# forward_auth_secret_header = "x-vmcp-forward-auth"
group_scopes = { "mcp-users" = "mcp:use", "mcp-admins" = "mcp:admin" }
```

Forward-auth rules:

1. Hop trust: the TCP peer must be in `trusted_proxies` and/or supply the header containing `forward_auth_secret` (if both are configured, both must match). With neither control configured, the configuration fails to load.
2. A missing `X-authentik-username` is rejected; the request does not become anonymous.
3. Groups are split on `|`, `,`, `;`, or whitespace and compared **exactly** (`architect-x` ≠ `architect`).
4. Scopes are derived from `group_scopes` on **every** request.

Spoofing `X-authentik-*` through `kubectl port-forward` or direct pod access **must not** create a session. See [ADR 0001](adr/0001-forward-auth-trust-and-identity-propagation.md).

Prefer a pre-registered public client in Authentik with Authorization Code + PKCE, rather than DCR.

### `/admin` authentication: `none` | `basic` | `authentik`

This is independent of the MCP `provider`:

| `auth.admin.mode` | Access mechanism |
| ----------------- | ---------------- |
| `none` | no checks (local use only) |
| `basic` (default) | HTTP Basic `login:password` → `master_password_argon2` |
| `authentik` | `X-authentik-username` / `X-authentik-groups` headers; requires an exact group from `required_groups` |

```toml
[auth.admin]
mode = "authentik"
required_groups = ["mcp-admins"]
# username_header / groups_header are optional; otherwise values come from [auth.authentik]
```

With `mode = authentik`, a missing username header returns 401. Groups are compared **exactly** after splitting on `|`, `,`, `;`, or whitespace (`architect-x` ≠ `architect`).

---

## OAuth flow (local)

```
1. GET  /.well-known/oauth-authorization-server   # discovery
2. POST /register                                 # DCR → client_id
3. GET  /authorize?...&code_challenge=S256...      # → /consent?cs=…
4. GET  /consent?cs=…                              # HTML form
5. POST /consent  password=<master>               # → redirect ?code=…
6. POST /token  grant_type=authorization_code …    # → access_token JWT
7. POST /mcp  Authorization: Bearer <token>
```

PKCE is required (`S256`). When RFC 8707 is supported, add `resource=https://<host>/mcp` (or `/mcp-proxy` when the proxy is enabled).

<a id="scripted-smoke-test"></a>
<details>
<summary>Scripted smoke test</summary>

```bash
BASE=https://gateway.example.com
REDIRECT=http://127.0.0.1:9999/callback

VERIFIER=$(openssl rand -base64 32 | tr -d '=+/' | tr '/+' '_-')
CHALLENGE=$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary | openssl base64 -A | tr -d '=' | tr '/+' '_-')

CLIENT=$(curl -fsS -X POST "$BASE/register" -H 'Content-Type: application/json' \
  -d "{\"client_name\":\"test\",\"redirect_uris\":[\"$REDIRECT\"]}" | jq -r .client_id)

LOC=$(curl -fsSI "$BASE/authorize?response_type=code&client_id=$CLIENT&redirect_uri=$REDIRECT&scope=mcp:use&code_challenge=$CHALLENGE&code_challenge_method=S256&resource=$BASE/mcp" | awk -F': ' '/^location:/I{print $2}' | tr -d '\r')
echo "Open in browser: $LOC"

# after entering the master password in the browser, you will receive ?code=… →
curl -fsS -X POST "$BASE/token" \
  -d "grant_type=authorization_code&code=$CODE&code_verifier=$VERIFIER&client_id=$CLIENT&redirect_uri=$REDIRECT&resource=$BASE/mcp"
```
</details>

---

## Master password

Generate a hash:

```bash
cargo run -p vmcp -- hash-password --password 'your-secret'
# on a VPS, use the image:
docker run --rm --entrypoint /usr/local/bin/vmcp ghcr.io/hewimetall/vmcp:latest \
  hash-password --password 'your-secret'
```

```toml
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$..."
```

Or use an environment variable (which overrides TOML): `VMCP_AUTH__MASTER_PASSWORD_ARGON2='$argon2id$...'`

The default in `vmcp.toml` is the hash of **`demo-master`** (for local use only).

**Consent:** an incorrect password returns `403`, but the session remains valid and can be retried. An expired `cs` returns `400`; start again at `/authorize`.

---

## Hash troubleshooting checklist

1. **`$` characters are not doubled in Docker `.env`** (the most common cause). Compose interpolates `$VAR`, so replace every `$` in the hash with `$$`:
   ```dotenv
   VMCP_MASTER_PASSWORD_ARGON2=$$argon2id$$v=19$$m=19456,t=2,p=1$$SALT$$HASH
   ```
   Then run `docker compose up -d --force-recreate vmcp`.

2. **Environment variables override TOML.** If `VMCP_AUTH__MASTER_PASSWORD_ARGON2` is set, even to an invalid value, the hash in the TOML file is ignored. Check it with:
   ```bash
   docker compose exec vmcp print-config | rg master_password
   ```

3. **The password is wrong.** A hash corresponds to exactly one password. If the password is lost, generate a new hash and redeploy.

4. **The password contains unintended characters:** `echo 'secret' | ...` appends `\n`; use `--password`. Also check autofill, keyboard layout, and whether `.env` changed without the container being recreated.

5. **A placeholder hash** such as `$REPLACE_ME` causes startup to fail with `not a valid argon2 hash`.

6. **`auth.enabled = false`** disables consent and bearer authentication entirely (localhost only).

---

## Static bearer tokens (`pre-reg`)

Use these for CI or scripts that cannot repeat OAuth after every restart:

```bash
cargo run -p vmcp -- pre-reg --name ci --scope mcp:use --out ./tokens.json
# → vmcp_<random>
```

```toml
[auth]
tokens_file = "./tokens.json"
```

```bash
curl -H "Authorization: Bearer vmcp_…" https://gateway.example.com/mcp
```

They do not expire (to revoke one, delete its row), are hot-reloaded without a restart, and work alongside OAuth.

For operators and Kubernetes, the HTTP API described below is more convenient than editing the file manually.

### Scopes (enforced)

The `scope` string is space-separated. It is enforced for `query_graphql`, `/mcp-proxy`, and `run_task`:

| Scope | Meaning |
| ----- | ------- |
| `mcp:use` | Full access (the previous behavior and the `pre-reg` default) |
| `mcp:admin` | Control-plane `/api/v1` + full MCP access |
| `mcp:read` | Query/read-only tools only |
| `mcp:write` | Query + Mutation |
| `upstream:<name>` | Allowlists a GraphQL namespace/proxy server (the presence of any such scope enables allowlist mode) |
| `deny:<server>.<tool>` | Denies a specific tool in addition to the grant rules |

Example: with `--scope 'mcp:use upstream:time'`, the agent cannot call `postgres.*`.

> **G25:** With an allowlist (`upstream:<name>` without `mcp:admin`), the catalog
> is filtered by the same predicate as calls: GraphQL `servers` / `search` /
> `prompts` / `searchPrompts` / `__type` (Query/Mutation namespace fields),
> GraphQL `notifications.source`, and `/mcp-proxy` `tools/list` + `prompts/list`.
> `mcp:admin` sees the full catalog. Tokens without any `upstream:*` scope still
> see the full catalog, matching the call-grant behavior. Contract: [ADR 0002](adr/0002-per-caller-catalog-visibility.md).

> **G30:** DCR / OAuth consent **never** grants `mcp:admin`; it is stripped.
> Admin access is available only through `pre-reg` or `/api/v1/tokens` with an operator bearer token.

Static tokens **never expire** (G34); the API has no last-used timestamp or TTL. Store
`tokens.json` as a secret. Rotate a token with `PUT /api/v1/tokens/:client_id`.
The file is limited to approximately 2 MiB / 10,000 entries (G33).

`/api/v1` is mounted **only** when `auth.enabled = true`, and Token CRUD requires a `tokens_file` (G13).

---

## Operator API `/api/v1` (Bearer)

This API is available alongside `/admin` (HTTP Basic). Automation accesses it with a static bearer token whose `scope` contains **`mcp:admin`**.

Bootstrap:

```bash
vmcp pre-reg --name operator --scope mcp:admin --out ./tokens.json
```

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/api/v1/tokens` | List tokens without the full secret (`token_prefix`) |
| `POST` | `/api/v1/tokens` | `{ "name", "scope"? }` → full `token` **once**; duplicate `name` → 409; unknown scope tokens → 400 |
| `PUT` | `/api/v1/tokens/:client_id` | Rotate the secret (same name/scope); returns the full `token` once |
| `DELETE` | `/api/v1/tokens/:client_id` | Revoke; deleting the final `mcp:admin` token is rejected with 400 |
| `GET` | `/api/v1/upstreams` | Status of the live pool |
| `POST` | `/api/v1/upstreams/reload` | Reconcile `registry.json` without a restart; the response includes `registry_sha256` / `mtime_unix_ms` |

```bash
curl -H "Authorization: Bearer $OPERATOR_TOKEN" \
  -H 'content-type: application/json' \
  https://gateway.example.com/api/v1/tokens \
  -d '{"name":"agent-a","scope":"mcp:use"}'
```

No bearer token → 401; a bearer token with `mcp:use` but without `mcp:admin` → 403.<br>
The `/admin` SPA and Basic authentication are **unchanged**.

---

## DCR clients (survive restart)

`POST /register` writes every client to **SQLite** (`auth.clients_db_path`, default `state/clients.db`) and to a hot DashMap cache. The store is reloaded after a restart, so Cursor does not receive `unknown client_id`.

### DCR policy

```toml
[auth]
dcr_enabled = true          # false → POST /register = 403 (pre-reg tokens remain valid)
dcr_max_clients = 256       # 0 = unlimited
dcr_redirect_uri_allowlist = ["http://127.0.0.1", "http://localhost", "cursor://"]
```

An empty allowlist retains the previous behavior and permits any `redirect_uri`. Rate-limit `/register` at Envoy; vmcp provides the policy controls above. Successful registrations are written to the audit log (`DCR client registered`).

Draft Kubernetes manifests: [`deploy/k8s/`](../deploy/k8s/).

Each registration receives a unique `name` (`cursor`, `cursor-2`, …). Rename it in the admin UI or with:

```bash
curl -X PATCH https://<domain>/admin/api/sessions/<client_id> \
  -u "admin:$MASTER_PASSWORD" -H "Content-Type: application/json" \
  -d '{"name":"laptop"}'
```

`name` must match `^[a-z0-9_-]{1,64}$` and be unique among DCR clients.

> **Upgrading from a version earlier than 1.0:** the `name` column migration has been removed. Delete `clients.db` and repeat DCR/consent, or the old SQLite database might fail to open.

```toml
[auth]
clients_db_path = "./state/clients.db"
```

### Data that survives a restart

| Data | Survives? |
| ---- | --------- |
| DCR `client_id` + `name` (SQLite) | **Yes** |
| Static `vmcp_…` tokens | **Yes** |
| JWT access tokens (in-memory JWKS) | **No** — repeat the token exchange |
| Authorization codes / consent sessions | **No** — restart the OAuth flow |

In Docker, mount the parent directory as writable (the `vmcp_state` volume).

---

## Disabling authentication (local use only)

```toml
[auth]
enabled = false
```

The bearer middleware is not mounted, and `/admin` is hidden. **Do not use this on a public network.**

The demo environment is already configured this way: [`demo/vmcp.toml`](../demo/vmcp.toml)
(`./vmcp --config ./demo/vmcp.toml`).

---

## JWT

- JWTs are signed with a rotating RS256 key (`jwks_rotate_secs`, default 86400).
- `token_ttl_secs` defaults to 3600; `jwks_rotate_secs` must be at least `2 * token_ttl_secs`.
- Rotation retains the previous `kid` (a two-key window), so unexpired JWTs remain valid.
- **By default, restart = new JWKS**, invalidating old JWTs. Use static tokens for automation.
- You can optionally persist the key on a PVC:

```toml
[auth]
jwks_private_key_pem_path = "/state/jwks.pem"  # load or generate+write 0600
```

With this setting, JWTs survive a restart. vmcp writes a PKCS#1 PEM and an atomic
`<path>.bundle.json` containing the **current and previous** keys, so the rotation
window survives pod recreation (G14/G31). Environment variable: `VMCP_AUTH__JWKS_PRIVATE_KEY_PEM_PATH`.
