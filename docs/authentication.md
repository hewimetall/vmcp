# Аутентификация

`/mcp` (и `/mcp-proxy`) защищены через **auth facade**: либо встроенный OAuth 2.1 AS/RS (`provider = "local"`), либо внешний [Authentik](https://github.com/goauthentik/authentik) (`provider = "authentik"`). `/admin` — отдельный фасад с тремя режимами.

## Поверхности

| Path | Auth | Назначение |
| ---- | ---- | ---------- |
| `/mcp` | Bearer JWT / `vmcp_…` / Authentik forward-auth | MCP streamable HTTP |
| `/mcp-proxy` | то же (если `[proxy]`) | Transparent upstream tools |
| `/admin` | `none` \| HTTP Basic \| Authentik headers | Operator SPA |
| `/health` | нет | Liveness |
| `/ready` | нет | Readiness (soft: ≥1 connected upstream, если в registry есть enabled) |
| `/authorize`, `/consent`, `/token`, `/register`, `/.well-known/*` | нет | OAuth + metadata (local AS) |

---

## Auth facade: `local` vs `authentik`

Одна точка входа (`AuthFacade`) на каждый защищённый запрос. Аноним / роль по умолчанию **никогда** не подставляются.

| | `provider = "local"` (default) | `provider = "authentik"` |
| - | --- | --- |
| Кто выдаёт токены | vmcp (DCR + PKCE + consent) | Authentik OAuth2/OIDC |
| MCP-клиент | Bearer JWT или `vmcp_…` | Bearer JWT от Authentik |
| Браузер за шлюзом | — | `X-authentik-username` + `X-authentik-groups` |
| JWT verify | локальный JWKS | remote JWKS (rustls `reqwest`) + Authentik JWKS |
| Local `/authorize`… | да | нет (только PRM → Authentik) |

### Authentik (рекомендуемая схема без DCR)

```toml
[auth]
enabled = true
provider = "authentik"
# master_password_argon2 опционален — только если нужен /admin Basic

[auth.authentik]
issuer = "https://auth.example.com/application/o/mcp-internal/"
jwks_url = "https://auth.example.com/application/o/mcp-internal/jwks/"
# пусто → public_base_url + /mcp (+ /mcp-proxy)
audiences = ["https://architecture.mcpwork.space/mcp"]
accept_bearer = true          # MCP-клиенты
forward_auth = true           # браузер за Envoy/Caddy forward-auth
# ОБЯЗАТЕЛЬНО при forward_auth: hop trust (иначе X-authentik-* с любого peer)
trusted_proxies = ["10.244.0.0/16"]
# или / плюс: forward_auth_secret = "…"  (env: VMCP_AUTH__AUTHENTIK__FORWARD_AUTH_SECRET)
# forward_auth_secret_header = "x-vmcp-forward-auth"
group_scopes = { "mcp-users" = "mcp:use", "mcp-admins" = "mcp:admin" }
```

Правила forward-auth:

1. Hop trust: TCP peer ∈ `trusted_proxies` и/или заголовок с `forward_auth_secret` (оба → AND). Без knobs — отказ при загрузке конфига.
2. Нет `X-authentik-username` → отказ (не аноним).
3. Группы режутся по `|`, `,`, `;`, пробелу; сравнение **точное** (`architect-x` ≠ `architect`).
4. Scope из `group_scopes` считается на **каждом** запросе.

Подделка `X-authentik-*` с `kubectl port-forward` / прямого доступа к pod **не** должна давать сессию — см. [ADR 0001](adr/0001-forward-auth-trust-and-identity-propagation.md).

Предпочтительно: pre-registered public client в Authentik + Authorization Code + PKCE (не DCR).

### `/admin` auth: `none` | `basic` | `authentik`

Независимо от MCP `provider`:

| `auth.admin.mode` | Как пускает |
| ----------------- | ----------- |
| `none` | без проверки (только локально) |
| `basic` (default) | HTTP Basic `login:password` → `master_password_argon2` |
| `authentik` | заголовки `X-authentik-username` / `X-authentik-groups`; нужна точная группа из `required_groups` |

```toml
[auth.admin]
mode = "authentik"
required_groups = ["mcp-admins"]
# username_header / groups_header — опционально; иначе из [auth.authentik]
```

При `mode = authentik`: нет username-заголовка → 401; группа сравнивается **точно** после split по `|`, `,`, `;`, пробелу (`architect-x` ≠ `architect`).

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

Для operator/k8s удобнее HTTP API (см. ниже), чем править файл вручную.

### Scopes (enforced)

Строка `scope` — space-separated. Проверяется на `query_graphql` / `/mcp-proxy` / `run_task`:

| Scope | Значение |
| ----- | -------- |
| `mcp:use` | Полный доступ (как раньше; default у `pre-reg`) |
| `mcp:admin` | Control-plane `/api/v1` + полный MCP доступ |
| `mcp:read` | Только Query / readOnly tools |
| `mcp:write` | Query + Mutation |
| `upstream:<name>` | Whitelist GraphQL namespace / proxy server (если есть хоть один — режим whitelist) |
| `deny:<server>.<tool>` | Запрет конкретного tool поверх grants |

Пример: `--scope 'mcp:use upstream:time'` — агент не вызовет `postgres.*`.

> **G25:** enforce режет **вызовы**. GraphQL schema / `search` / introspection пока могут
> **показывать** чужие namespaces — не считать scopes полной изоляцией каталога.

> **G30:** DCR / OAuth consent **не** выдают `mcp:admin` (strips). Admin только через
> `pre-reg` / `/api/v1/tokens` с operator Bearer.

Static tokens **бессрочные** (G34); нет last-used/TTL в API. Храни `tokens.json` как secret;
rotate: `PUT /api/v1/tokens/:client_id`. Файл max ~2 MiB / 10k entries (G33).

`/api/v1` монтируется **только** при `auth.enabled` + нужен `tokens_file` для Token CRUD (G13).

---

## Operator API `/api/v1` (Bearer)

Параллельно `/admin` (HTTP Basic). Automation ходит сюда с static bearer, у которого в `scope` есть **`mcp:admin`**.

Bootstrap:

```bash
vmcp pre-reg --name operator --scope mcp:admin --out ./tokens.json
```

| Method | Path | Описание |
| ------ | ---- | -------- |
| `GET` | `/api/v1/tokens` | Список без полного секрета (`token_prefix`) |
| `POST` | `/api/v1/tokens` | `{ "name", "scope"? }` → полный `token` **один раз**; duplicate `name` → 409; unknown scope tokens → 400 |
| `PUT` | `/api/v1/tokens/:client_id` | Rotate secret (same name/scope); полный `token` один раз |
| `DELETE` | `/api/v1/tokens/:client_id` | Revoke; нельзя удалить последний `mcp:admin` → 400 |
| `GET` | `/api/v1/upstreams` | Status live pool |
| `POST` | `/api/v1/upstreams/reload` | Reconcile `registry.json` без рестарта; ответ включает `registry_sha256` / `mtime_unix_ms` |

```bash
curl -H "Authorization: Bearer $OPERATOR_TOKEN" \
  -H 'content-type: application/json' \
  https://gateway.example.com/api/v1/tokens \
  -d '{"name":"agent-a","scope":"mcp:use"}'
```

Без Bearer → 401; Bearer с `mcp:use` (без `mcp:admin`) → 403.  
`/admin` SPA + Basic **не менялись**.

---

## DCR clients (переживают restart)

`POST /register` пишет каждый client в **SQLite** (`auth.clients_db_path`, default `state/clients.db`) + hot cache в DashMap. После рестарта store перечитывается — Cursor не ловит `unknown client_id`.

### DCR policy

```toml
[auth]
dcr_enabled = true          # false → POST /register = 403 (pre-reg tokens остаются)
dcr_max_clients = 256       # 0 = без лимита
dcr_redirect_uri_allowlist = ["http://127.0.0.1", "http://localhost", "cursor://"]
```

Пустой allowlist = как раньше (любой `redirect_uri`). Rate-limit на `/register` лучше на Envoy; vmcp даёт policy hooks выше. Успешные registration пишутся в audit log (`DCR client registered`).

Черновые k8s манифесты: [`deploy/k8s/`](../deploy/k8s/).

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

- Подписаны ротируемым RS256 ключом (`jwks_rotate_secs`, default 86400).
- `token_ttl_secs` default 3600; должно быть `jwks_rotate_secs >= 2 * token_ttl_secs`.
- Rotation держит предыдущий `kid` (окно из 2 ключей) — неистёкшие JWT ещё принимаются.
- **По умолчанию restart = новый JWKS** → старые JWT мертвы. Для automation бери static tokens.
- Опционально persist ключа на PVC:

```toml
[auth]
jwks_private_key_pem_path = "/state/jwks.pem"  # load or generate+write 0600
```

Тогда JWT переживают restart. Пишется PKCS#1 PEM + атомарный
`<path>.bundle.json` с **current и previous** ключами (окно ротации переживает
pod recreate — G14/G31). Env: `VMCP_AUTH__JWKS_PRIVATE_KEY_PEM_PATH`.
