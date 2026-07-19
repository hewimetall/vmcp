# Развертывание

vmcp — один Rust binary. В production ставь за TLS (Caddy/nginx) и направляй клиентов на `https://<domain>/mcp`.

## Режимы

| Режим | Как | Auth | Admin |
| ----- | --- | ---- | ----- |
| HTTP gateway (default) | `vmcp serve`, порт 8765 | OAuth + bearer | `/admin` |
| Auth disabled | `VMCP_AUTH__ENABLED=false` | нет (только локально) | скрыт |

Для stdio (Claude Desktop, Cursor) — отдельный [vmcp-lite](https://github.com/hewimetall/vmcp-lite).

## Артефакты

Тег `v*` собирает через CI:
- Binaries (Linux/Windows/macOS) → GitHub Release
- Docker image → `ghcr.io/hewimetall/vmcp:<version>`

**На VPS качай GHCR image, не собирай на сервере.**

---

## Docker Compose (рекомендуется)

Stack: **vmcp** (GHCR image) + **Caddy** (TLS на 80/443).

**Нужно:** DNS A-record на IP сервера, Docker Compose v2, открытые порты 80/443.

### Bootstrap

```bash
git clone https://github.com/hewimetall/vmcp.git && cd vmcp
./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0 --password 'your-secret'
```

Script копирует `.env`, прописывает домен/image, генерит argon2 hash внутри image (с удвоенными `$`), делает `docker pull` + `up -d`.

Проверка:

```bash
curl -fsS https://gateway.example.com/health   # → ok
```

### Ручной .env (если не через bootstrap)

```dotenv
VMCP_DOMAIN=gateway.example.com
VMCP_IMAGE=ghcr.io/hewimetall/vmcp:1.0.0
VMCP_MASTER_PASSWORD_ARGON2=$$argon2id$$v=19$$m=19456,t=2,p=1$$SALT$$DIGEST
```

> ⚠️ **Каждый `$` в hash удваивай как `$$`** — Compose иначе съест как переменную и молча испортит hash. Генерь hash тем же image:
> ```bash
> docker run --rm --entrypoint /usr/local/bin/vmcp ghcr.io/hewimetall/vmcp:1.0.0 \
>   hash-password --password 'your-secret'
> ```

```bash
docker compose pull && docker compose up -d
```

Проверить, что процесс видит правильный hash:

```bash
docker compose up -d --force-recreate vmcp
docker compose exec vmcp print-config | rg master_password
```

### Что задаёт compose

`VMCP_IMAGE`, `VMCP_HOST=0.0.0.0`, `VMCP_PUBLIC_BASE_URL=https://${VMCP_DOMAIN}`, `VMCP_AUTH__ISSUER`, master password из `.env`, registry из `/data`, sessions в named volume.

`vmcp.toml` монтируется read-only; **прод-настройки идут через env**.

### Production upstreams

Дефолтный mount `./demo:/data` — файлы registry/specs/skills с демо-стенда.
Runtime image **без** Node/`uv`, поэтому stdio-upstreams из `demo/registry.json`
в контейнере не поднимутся: на VPS монтируй свой data-каталог; локальный demo —
через бинарь/`cargo` и [`demo/vmcp.toml`](../demo/vmcp.toml):

```bash
./vmcp --config ./demo/vmcp.toml
```

```yaml
volumes:
  - ./vmcp.toml:/vmcp.toml:ro
  - ./prod-data:/data:ro   # registry.json, specs/, skills/
```

Детали: [upstreams.md](upstreams.md), [skills.md](skills.md), [demo/README.md](../demo/README.md).

### Локальная сборка (dev)

```bash
./deploy/bootstrap.sh --domain gateway.example.com --build --password 'your-secret'
```

---

## Bare metal (без Docker)

Скачай binary из Release (лучше, чем собирать на VPS):

```bash
curl -fsSL -o vmcp.tgz \
  "https://github.com/hewimetall/vmcp/releases/download/v1.0.0/vmcp-1.0.0-linux-x86_64.tar.gz"
tar -xzf vmcp.tgz
install -m 755 vmcp /opt/vmcp/vmcp
```

Или из исходников: `cargo build --release -p vmcp` (`--no-default-features` — без admin).

### Конфиг

```toml
host = "0.0.0.0"
port = 8765
public_base_url = "https://gateway.example.com"

[auth]
issuer = "https://gateway.example.com"
master_password_argon2 = "$argon2id$..."   # from `vmcp hash-password`
```

Или env (в shell оборачивай hash в **single quotes**):

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

Secrets в `vmcp.env` (`chmod 600`). Вне Compose `$$` не нужны.

### TLS

Terminate на Caddy/nginx, proxy на `127.0.0.1:8765`. `public_base_url` и `auth.issuer` **обязаны** быть публичным `https://` — иначе OAuth-клиенты отвалятся.

---

## Чеклист production

- [ ] Deploy tag `v*` (или `--build` для staging)
- [ ] Уникальный master password hash
- [ ] `public_base_url` + `auth.issuer` = публичный HTTPS
- [ ] В Docker `.env` — удвоенные `$$` в hash
- [ ] `VMCP_IMAGE` закреплён на версии (`:1.0.0`, не `:latest`)
- [ ] Проверен prod `registry.json` (локальный стенд — [`demo/README.md`](../demo/README.md) / [`demo/vmcp.toml`](../demo/vmcp.toml))
- [ ] CI tokens через `vmcp pre-reg` → `auth.tokens_file` (если нужно)
- [ ] Writable volume для `tasks.db_path` (если `[tasks]`) — [tasks.md](tasks.md)
- [ ] Writable volume для `recorder.sessions_dir` — [sessions.md](sessions.md)
- [ ] `RUST_LOG=info`
- [ ] Проверен `/health` + один OAuth consent
- [ ] **Никогда** `auth.enabled = false` в публичной сети

---

## Обновления

```bash
./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0
# или: правишь VMCP_IMAGE в .env →
docker compose pull && docker compose up -d --force-recreate vmcp
```

Важно при рестарте:
- **JWT инвалидируются** (JWKS в памяти) — клиенты переделывают token exchange.
- **DCR client_id сохраняются** в SQLite (`auth.clients_db_path`) — Cursor не нужно re-register.
- Чтобы полностью пропускать OAuth после redeploy — [static tokens](authentication.md#static-bearer-tokens-pre-reg).

Сохраняй `auth.clients_db_path` и `recorder.sessions_dir` между пересозданиями.
