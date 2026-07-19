# Аутентификация

`/mcp` (и `/mcp-proxy`) — OAuth 2.1 + PKCE + DCR, принимают Bearer JWT или static `vmcp_…` token. `/admin` — отдельно, **HTTP Basic** против master password (не bearer).

## Поверхности

| Path | Auth | Назначение |
| ---- | ---- | ---------- |
| `/mcp` | Bearer JWT или `vmcp_…` | MCP streamable HTTP |
| `/mcp-proxy` | то же (если `[proxy]`) | Transparent upstream tools |
| `/admin` | HTTP Basic (master password) | Operator SPA |
| `/health` | нет | Liveness |
| `/authorize`, `/consent`, `/token`, `/register`, `/.well-known/*` | нет | OAuth + metadata |

---

## OAuth flow

```
1. GET  /.well-known/oauth-authorization-server   # discovery
2. POST /register                                 # DCR → client_id
3. GET  /authorize?...&code_challenge=S256...      # → /consent?cs=…
4. GET  /consent?cs=…                              # HTML form
5. POST /consent  password=<master>               # → redirect ?code=…
6. POST /token  grant_type=authorization_code …    # → access_token JWT
7. POST /mcp  Authorization: Bearer <token>
```

PKCE обязателен (`S256`). Если поддерживается RFC 8707 — добавляй `resource=https://<host>/mcp` (или `/mcp-proxy` при включённом proxy).

<details>
<summary>Скриптовый smoke test</summary>

```bash
BASE=https://gateway.example.com
REDIRECT=http://127.0.0.1:9999/callback

VERIFIER=$(openssl rand -base64 32 | tr -d '=+/' | tr '/+' '_-')
CHALLENGE=$(printf '%s' "$VERIFIER" | openssl dgst -sha256 -binary | openssl base64 -A | tr -d '=' | tr '/+' '_-')

CLIENT=$(curl -fsS -X POST "$BASE/register" -H 'Content-Type: application/json' \
  -d "{\"client_name\":\"test\",\"redirect_uris\":[\"$REDIRECT\"]}" | jq -r .client_id)

LOC=$(curl -fsSI "$BASE/authorize?response_type=code&client_id=$CLIENT&redirect_uri=$REDIRECT&scope=mcp:use&code_challenge=$CHALLENGE&code_challenge_method=S256&resource=$BASE/mcp" | awk -F': ' '/^location:/I{print $2}' | tr -d '\r')
echo "Open in browser: $LOC"

# после ввода master password в браузере получишь ?code=… →
curl -fsS -X POST "$BASE/token" \
  -d "grant_type=authorization_code&code=$CODE&code_verifier=$VERIFIER&client_id=$CLIENT&redirect_uri=$REDIRECT&resource=$BASE/mcp"
```
</details>

---

## Master password

Сгенерировать hash:

```bash
cargo run -p vmcp -- hash-password --password 'your-secret'
# на VPS — через image:
docker run --rm --entrypoint /usr/local/bin/vmcp ghcr.io/hewimetall/vmcp:latest \
  hash-password --password 'your-secret'
```

```toml
[auth]
master_password_argon2 = "$argon2id$v=19$m=19456,t=2,p=1$..."
```

Или env (переопределяет TOML): `VMCP_AUTH__MASTER_PASSWORD_ARGON2='$argon2id$...'`

Дефолт в `vmcp.toml` — hash от **`demo-master`** (только локально).

**Consent:** неверный пароль → `403`, session жива, можно повторить. Протух `cs` → `400`, начинай с `/authorize`.

---

## Hash не работает — чеклист

1. **`$` в Docker `.env` не удвоены** (самое частое). Compose интерполирует `$VAR`, поэтому каждый `$` в hash → `$$`:
   ```dotenv
   VMCP_MASTER_PASSWORD_ARGON2=$$argon2id$$v=19$$m=19456,t=2,p=1$$SALT$$HASH
   ```
   Затем `docker compose up -d --force-recreate vmcp`.

2. **Env переопределяет TOML.** Если задан `VMCP_AUTH__MASTER_PASSWORD_ARGON2` (даже кривой) — hash из toml игнорится. Проверь:
   ```bash
   docker compose exec vmcp print-config | rg master_password
   ```

3. **Не тот пароль.** Hash соответствует ровно одному паролю. Перегенери и redeploy, если потерял.

4. **Мусор в пароле:** `echo 'secret' | ...` добавляет `\n` — используй `--password`. Ещё: autofill, раскладка, `.env` изменён но container не пересоздан.

5. **Placeholder hash** (`$REPLACE_ME` и т.п.) → boot падает с `not a valid argon2 hash`.

6. **`auth.enabled = false`** — нет consent/bearer вообще (только localhost).

---

## Static bearer tokens (`pre-reg`)

Для CI/скриптов, которым не подходит OAuth на каждый рестарт:

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

Не истекают (revoke = удали строку), hot-reload без рестарта, OAuth работает параллельно.

---

## DCR clients (переживают restart)

`POST /register` пишет каждый client в **SQLite** (`auth.clients_db_path`, default `state/clients.db`) + hot cache в DashMap. После рестарта store перечитывается — Cursor не ловит `unknown client_id`.

Каждая registration получает уникальное `name` (`cursor`, `cursor-2`, …). Переименовать в admin UI или:

```bash
curl -X PATCH https://<domain>/admin/api/sessions/<client_id> \
  -u "admin:$MASTER_PASSWORD" -H "Content-Type: application/json" \
  -d '{"name":"laptop"}'
```

`name` — `^[a-z0-9_-]{1,64}$`, уникален среди DCR clients.

> **Upgrade <1.0:** миграция колонки `name` удалена. Удали `clients.db` и переделай DCR/consent — иначе старая SQLite может не открыться.

```toml
[auth]
clients_db_path = "./state/clients.db"
```

### Что переживает restart

| Данные | Переживает? |
| ------ | ----------- |
| DCR `client_id` + `name` (SQLite) | **Да** |
| Static `vmcp_…` tokens | **Да** |
| JWT access tokens (in-memory JWKS) | **Нет** — повтори token exchange |
| Auth codes / consent sessions | **Нет** — начни OAuth заново |

В Docker монтируй parent dir как writable (volume `vmcp_state`).

---

## Отключение auth (только локально)

```toml
[auth]
enabled = false
```

Bearer middleware не монтируется, `/admin` скрывается. **Не в публичной сети.**

Демо-стенд уже так настроен: [`demo/vmcp.toml`](../demo/vmcp.toml)
(`./vmcp --config ./demo/vmcp.toml`).

---

## JWT

- Подписаны ротируемым in-memory ключом (`jwks_rotate_secs`, default 86400).
- `token_ttl_secs` default 3600; должно быть `jwks_rotate_secs >= 2 * token_ttl_secs`.
- Rotation держит предыдущий `kid` (окно из 2 ключей) — неистёкшие JWT ещё принимаются.
- **Restart = новый JWKS** → старые JWT мертвы. Для automation бери static tokens.
