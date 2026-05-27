#!/usr/bin/env bash
# Deploy script — runs ON the target server inside a checkout of the repo
# at $VMCP_HOME (default ~/vmcp). Either `git clone`d (preferred — the
# prod server already has the SSH key for hewimetall/codefest2026vmcp)
# or rsynced (fallback, see infra/rsync-exclude.txt).
#
# Prerequisites on the server:
#   - docker + docker compose v2 (run `bash infra/setup-docker.sh` if missing)
#   - ports 80 + 443 free (Caddy binds them)
#   - DNS A record: $VMCP_DOMAIN → this host
#   - infra/compose/.env filled in (copy from infra/.env.example)
#
# First deploy on a clean server:
#   ssh root@185.142.99.223
#   git clone git@github.com:hewimetall/codefest2026vmcp.git ~/vmcp
#   cd ~/vmcp
#   # build once to get vmcp:source for hash-password
#   docker compose -f infra/compose/docker-compose.yml \
#                  -f infra/compose/docker-compose.deploy.yml build vmcp
#   docker run --rm -it vmcp:source hash-password   # paste hash into .env
#   cp infra/.env.example infra/compose/.env && vi infra/compose/.env
#   bash infra/deploy.sh
#
# Redeploy (update to latest main):
#   ssh root@185.142.99.223 'cd ~/vmcp && git pull && bash infra/deploy.sh'

set -euo pipefail

VMCP_HOME="${VMCP_HOME:-$HOME/vmcp}"
COMPOSE_DIR="$VMCP_HOME/infra/compose"
COMPOSE_FILES=(
    -f docker-compose.yml
    -f docker-compose.deploy.yml
)

# 1. Layout sanity
if [[ ! -f "$COMPOSE_DIR/docker-compose.yml" ]]; then
    echo "ERROR: $COMPOSE_DIR/docker-compose.yml not found. Did rsync land in $VMCP_HOME ?" >&2
    exit 1
fi

# 2. Docker sanity
if ! command -v docker >/dev/null; then
    echo "ERROR: docker not installed. Run 'bash infra/setup-docker.sh' first." >&2
    exit 1
fi
if ! docker compose version >/dev/null 2>&1; then
    echo "ERROR: docker compose v2 plugin missing." >&2
    exit 1
fi

# 3. .env sanity
if [[ ! -f "$COMPOSE_DIR/.env" ]]; then
    echo "ERROR: $COMPOSE_DIR/.env missing." >&2
    echo "       cp $VMCP_HOME/infra/.env.example $COMPOSE_DIR/.env && \$EDITOR \$_" >&2
    exit 1
fi

set -a; source "$COMPOSE_DIR/.env"; set +a
required=(VMCP_DOMAIN VMCP_ACME_EMAIL VMCP_MASTER_PASSWORD_HASH)
missing=()
for v in "${required[@]}"; do
    [[ -z "${!v:-}" ]] && missing+=("$v")
done
if (( ${#missing[@]} > 0 )); then
    echo "ERROR: empty required env vars: ${missing[*]}" >&2
    exit 1
fi
if [[ "$VMCP_MASTER_PASSWORD_HASH" != \$argon2id\$* ]]; then
    echo "ERROR: VMCP_MASTER_PASSWORD_HASH doesn't look like an argon2id hash." >&2
    echo "       Generate one with: docker run --rm vmcp:source hash-password" >&2
    exit 1
fi

cd "$COMPOSE_DIR"

# 4. Build + start.
echo "==> Building vmcp image on host (first run ≈ 6-10 min for the Rust workspace)"
docker compose "${COMPOSE_FILES[@]}" build vmcp

echo "==> Starting stack: vmcp + postgres + dagger-engine + caddy"
docker compose "${COMPOSE_FILES[@]}" up -d

# 5. Wait for postgres seed.
echo "==> Waiting for postgres to seed demo data"
for i in 1 2 3 4 5 6 7 8 9 10; do
    if docker compose "${COMPOSE_FILES[@]}" exec -T postgres \
        psql -U demo -d demo -c 'SELECT count(*) FROM employees;' >/dev/null 2>&1; then
        echo "    postgres seed: OK"
        break
    fi
    sleep 3
done

# 6. Health check via Caddy (port 80 stays open for ACME HTTP-01).
echo "==> Health check via Caddy"
for i in 1 2 3 4 5 6; do
    if curl -fsS "http://localhost/health" >/dev/null 2>&1; then
        echo "    vmcp /health (via Caddy): OK"
        break
    fi
    sleep 5
done

# 7. Show ACME provisioning trace.
echo "==> Caddy log (Let's Encrypt provisioning for $VMCP_DOMAIN):"
docker compose "${COMPOSE_FILES[@]}" logs --tail=20 caddy

cat <<EOF

==> Done.

Try:
    curl -I https://${VMCP_DOMAIN}/health
    curl -s  https://${VMCP_DOMAIN}/.well-known/oauth-authorization-server | jq .

Logs:
    cd $COMPOSE_DIR && docker compose ${COMPOSE_FILES[*]} logs -f vmcp

Admin UI:
    https://${VMCP_DOMAIN}/admin   (HTTP Basic; password = the one you hashed)

EOF
