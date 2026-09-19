# Deployment

**Language:** English | [Русский](ru/deployment.md)

vmcp is a single Rust binary. In production, deploy it behind TLS (Caddy/nginx) and point clients to `https://<domain>/mcp`.

## Modes

| Mode | How | Auth | Admin |
| ---- | --- | ---- | ----- |
| HTTP gateway (default) | `vmcp serve`, port 8765 | OAuth + bearer | `/admin` |
| Auth disabled | `VMCP_AUTH__ENABLED=false` | none (local only) | hidden |

For stdio (Claude Desktop, Cursor), use the separate [vmcp-lite](https://github.com/hewimetall/vmcp-lite).

## Artifacts

CI builds the following for every `v*` tag:
- Binaries (linux-x86_64, windows-x86_64, macos-aarch64) → GitHub Release
- Docker image → `ghcr.io/hewimetall/vmcp:<version>`

**On a VPS, pull the GHCR image instead of building on the server.**

---

## Kubernetes (draft)

Gateway API manifests, a PVC, and probes: [`deploy/k8s/`](../deploy/k8s/).<br>
For operator reconciliation, edit `registry.json` or call `POST /api/v1/upstreams/reload`; manage tokens through `/api/v1/tokens` (Bearer `mcp:admin`).

## Docker Compose (recommended)

Stack: **vmcp** (GHCR image) + **Caddy** (TLS on ports 80/443).

**Requirements:** a DNS A record pointing to the server IP, Docker Compose v2, and open ports 80/443.

### Bootstrap

```bash
git clone https://github.com/hewimetall/vmcp.git && cd vmcp
./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0 --password 'your-secret'
```

The script copies `.env`, sets the domain and image, generates an Argon2 hash inside the image (with doubled `$` characters), then runs `docker pull` and `up -d`.

Verify the deployment:

```bash
curl -fsS https://gateway.example.com/health   # → ok
```

### Manual `.env` setup (without bootstrap)

```dotenv
VMCP_DOMAIN=gateway.example.com
VMCP_IMAGE=ghcr.io/hewimetall/vmcp:1.0.0
VMCP_MASTER_PASSWORD_ARGON2=$$argon2id$$v=19$$m=19456,t=2,p=1$$SALT$$DIGEST
```

> ⚠️ **Double every `$` in the hash as `$$`**. Otherwise, Compose treats it as a variable and silently corrupts the hash. Generate the hash with the same image:
> ```bash
> docker run --rm --entrypoint /usr/local/bin/vmcp ghcr.io/hewimetall/vmcp:1.0.0 \
>   hash-password --password 'your-secret'
> ```

```bash
docker compose pull && docker compose up -d
```

Check that the process sees the correct hash:

```bash
docker compose up -d --force-recreate vmcp
docker compose exec vmcp print-config | rg master_password
```

### What Compose configures

`VMCP_IMAGE`, `VMCP_HOST=0.0.0.0`, `VMCP_PUBLIC_BASE_URL=https://${VMCP_DOMAIN}`, `VMCP_AUTH__ISSUER`, the master password from `.env`, the registry from `/data`, and sessions in a named volume.

`vmcp.toml` is mounted read-only; **production settings are supplied through environment variables**.

### Production upstreams

The default `./demo:/data` mount provides registry/spec/skill files from the demo environment.
The runtime image contains **neither** Node nor `uv`, so the stdio upstreams from
`demo/registry.json` will not start inside the container. On a VPS, mount your own
data directory; run the demo locally with the binary/`cargo` and
[`demo/vmcp.toml`](../demo/vmcp.toml):

```bash
./vmcp --config ./demo/vmcp.toml
```

```yaml
volumes:
  - ./vmcp.toml:/vmcp.toml:ro
  - ./prod-data:/data:ro   # registry.json, specs/, skills/
```

For details, see [upstreams.md](upstreams.md), [skills.md](skills.md), and [demo/README.md](../demo/README.md).

### Local build (development)

```bash
./deploy/bootstrap.sh --domain gateway.example.com --build --password 'your-secret'
```

---

## Bare metal (without Docker)

Download the binary from a release (preferable to building on the VPS):

```bash
curl -fsSL -o vmcp.tgz \
  "https://github.com/hewimetall/vmcp/releases/download/v1.0.0/vmcp-1.0.0-linux-x86_64.tar.gz"
tar -xzf vmcp.tgz
install -m 755 vmcp /opt/vmcp/vmcp
```

Alternatively, build from source with `cargo build --release -p vmcp` (use `--no-default-features` to exclude the admin UI).

### Configuration

```toml
host = "0.0.0.0"
port = 8765
public_base_url = "https://gateway.example.com"

[auth]
issuer = "https://gateway.example.com"
master_password_argon2 = "$argon2id$..."   # from `vmcp hash-password`
```

Or use environment variables (wrap the hash in **single quotes** in the shell):

```bash
export VMCP_PUBLIC_BASE_URL=https://gateway.example.com
export VMCP_AUTH__ISSUER=https://gateway.example.com
export VMCP_AUTH__MASTER_PASSWORD_ARGON2='$argon2id$...'
```

### systemd

```ini
[Unit]
Description=vmcp MCP gateway
After=network.target

[Service]
Type=simple
User=vmcp
WorkingDirectory=/opt/vmcp
EnvironmentFile=/opt/vmcp/vmcp.env
ExecStart=/opt/vmcp/vmcp --config /opt/vmcp/vmcp.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Store secrets in `vmcp.env` (`chmod 600`). Outside Compose, you do not need to double `$` as `$$`.

### TLS

Terminate TLS at Caddy/nginx and proxy to `127.0.0.1:8765`. `public_base_url` and `auth.issuer` **must** be public `https://` URLs, or OAuth clients will fail.

---

## Production checklist

- [ ] Deploy a `v*` tag (or use `--build` for staging)
- [ ] Use a unique master password hash
- [ ] Set `public_base_url` and `auth.issuer` to the public HTTPS URL
- [ ] Double every `$` in the hash as `$$` in the Docker `.env` file
- [ ] Pin `VMCP_IMAGE` to a version (`:1.0.0`, not `:latest`)
- [ ] Verify the production `registry.json` (for a local environment, see [`demo/README.md`](../demo/README.md) / [`demo/vmcp.toml`](../demo/vmcp.toml))
- [ ] Create CI tokens with `vmcp pre-reg` and configure `auth.tokens_file` if needed
- [ ] Use a writable volume for `tasks.db_path` when `[tasks]` is enabled — see [tasks.md](tasks.md)
- [ ] Use a writable volume for `recorder.sessions_dir` — see [sessions.md](sessions.md)
- [ ] Set `RUST_LOG=info`
- [ ] Verify `/health` and complete one OAuth consent flow
- [ ] **Never** set `auth.enabled = false` on a public network

---

## Updates

```bash
./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0
# or: edit VMCP_IMAGE in .env, then run:
docker compose pull && docker compose up -d --force-recreate vmcp
```

Important behavior on restart:
- **JWTs are invalidated** (JWKS is held in memory), so clients must repeat the token exchange.
- **DCR client IDs are preserved** in SQLite (`auth.clients_db_path`), so Cursor does not need to register again.
- To bypass OAuth entirely after a redeploy, use [static tokens](authentication.md#static-bearer-tokens-pre-reg).

Persist `auth.clients_db_path` and `recorder.sessions_dir` across container recreations.
