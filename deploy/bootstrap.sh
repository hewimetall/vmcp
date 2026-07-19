#!/usr/bin/env bash
# Bootstrap a TLS-terminated vmcp stack from a GitHub Actions release image.
#
# The `release` workflow (`.github/workflows/release.yml`) builds and pushes
# `ghcr.io/<owner>/vmcp` on every `v*` tag. This script configures `.env` and
# starts docker compose against that published image — no local Rust build.
#
# Usage:
#   ./deploy/bootstrap.sh --domain gateway.example.com
#   ./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0
#   ./deploy/bootstrap.sh --domain gateway.example.com --password 'secret'
#   ./deploy/bootstrap.sh --domain gateway.example.com --build   # local Dockerfile
#
# Docs: docs/deployment.md

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

DOMAIN=""
TAG="latest"
IMAGE_OVERRIDE=""
PASSWORD=""
LOCAL_BUILD=0
SKIP_UP=0
COMPOSE=(docker compose)

DEFAULT_IMAGE_REPO="ghcr.io/hewimetall/vmcp"

usage() {
  cat <<'EOF'
Bootstrap a TLS-terminated vmcp stack from a GitHub Actions release image.

The `release` workflow builds and pushes ghcr.io/<owner>/vmcp on every v* tag.
This script configures .env and starts docker compose against that image.

Usage:
  ./deploy/bootstrap.sh --domain gateway.example.com
  ./deploy/bootstrap.sh --domain gateway.example.com --tag 1.0.0
  ./deploy/bootstrap.sh --domain gateway.example.com --password 'secret'
  ./deploy/bootstrap.sh --domain gateway.example.com --build
  ./deploy/bootstrap.sh --domain gateway.example.com --skip-up

Options:
  --domain HOST     Public DNS name (required)
  --tag TAG         GHCR tag (default: latest; leading v stripped)
  --image REF       Full image ref (overrides --tag / default repo)
  --password TEXT   Master password to hash into .env
  --build           Build from local Dockerfile instead of pulling GHCR
  --skip-up         Only write .env; do not pull/start containers
  -h, --help        Show this help

Docs: docs/deployment.md
EOF
  exit "${1:-0}"
}

die() { echo "error: $*" >&2; exit 1; }

require_docker() {
  command -v docker >/dev/null || die "docker is required"
  docker compose version >/dev/null 2>&1 || die "docker compose v2 is required"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage 0 ;;
    --domain) DOMAIN="${2:-}"; shift 2 ;;
    --tag) TAG="${2:-}"; shift 2 ;;
    --image) IMAGE_OVERRIDE="${2:-}"; shift 2 ;;
    --password) PASSWORD="${2:-}"; shift 2 ;;
    --build) LOCAL_BUILD=1; shift ;;
    --skip-up) SKIP_UP=1; shift ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

[[ -n "$DOMAIN" ]] || die "required: --domain <hostname> (DNS A record must point here)"

# Strip a leading `v` so --tag v1.0.0 and --tag 1.0.0 both work.
TAG="${TAG#v}"

if [[ -n "$IMAGE_OVERRIDE" ]]; then
  VMCP_IMAGE="$IMAGE_OVERRIDE"
elif [[ "$LOCAL_BUILD" -eq 1 ]]; then
  VMCP_IMAGE="vmcp:local"
else
  VMCP_IMAGE="${DEFAULT_IMAGE_REPO}:${TAG}"
fi

# --- .env ------------------------------------------------------------------

if [[ ! -f .env ]]; then
  cp .env.example .env
  echo "created .env from .env.example"
fi

# Upsert a KEY=VALUE line in .env (preserve unrelated keys / comments).
upsert_env() {
  local key="$1" value="$2" tmp
  tmp="$(mktemp)"
  awk -v k="$key" -v v="$value" '
    BEGIN { done=0 }
    $0 ~ "^" k "=" { print k "=" v; done=1; next }
    { print }
    END { if (!done) print k "=" v }
  ' .env > "$tmp"
  mv "$tmp" .env
}

upsert_env VMCP_DOMAIN "$DOMAIN"
upsert_env VMCP_IMAGE "$VMCP_IMAGE"

# --- master password hash --------------------------------------------------

need_hash=0
if grep -qE 'REPLACE_SALT|REPLACE_DIGEST|REPLACE_ME' .env; then
  need_hash=1
fi

if [[ -n "$PASSWORD" ]] || [[ "$need_hash" -eq 1 ]]; then
  if [[ -z "$PASSWORD" ]]; then
    if [[ -t 0 ]]; then
      read -r -s -p "Master password (plaintext, will be hashed): " PASSWORD
      echo
      [[ -n "$PASSWORD" ]] || die "empty password"
    else
      die "VMCP_MASTER_PASSWORD_ARGON2 still has placeholders; pass --password or edit .env"
    fi
  fi

  require_docker
  echo "generating argon2id hash via ${VMCP_IMAGE} …"
  if [[ "$LOCAL_BUILD" -eq 1 ]]; then
    "${COMPOSE[@]}" -f docker-compose.yml -f docker-compose.build.yml build vmcp
    HASH="$("${COMPOSE[@]}" -f docker-compose.yml -f docker-compose.build.yml \
      run --rm --no-deps vmcp hash-password --password "$PASSWORD")"
  else
    # Pull first so hash-password works even before `compose up`.
    if ! docker image inspect "$VMCP_IMAGE" >/dev/null 2>&1; then
      echo "pulling ${VMCP_IMAGE} …"
      if ! docker pull "$VMCP_IMAGE"; then
        die "failed to pull ${VMCP_IMAGE}. For private packages: docker login ghcr.io -u USERNAME. Or use --build."
      fi
    fi
    HASH="$(docker run --rm --entrypoint /usr/local/bin/vmcp "$VMCP_IMAGE" \
      hash-password --password "$PASSWORD")"
  fi

  # Compose interpolates $VAR — double every $ for .env.
  HASH_DOUBLED="${HASH//\$/\$\$}"
  upsert_env VMCP_MASTER_PASSWORD_ARGON2 "$HASH_DOUBLED"
  echo "wrote VMCP_MASTER_PASSWORD_ARGON2 to .env (\$ doubled for Compose)"
fi

# --- bring up --------------------------------------------------------------

if [[ "$SKIP_UP" -eq 1 ]]; then
  echo "configured .env (VMCP_DOMAIN=${DOMAIN}, VMCP_IMAGE=${VMCP_IMAGE}); skipped compose up"
  exit 0
fi

require_docker

if [[ "$LOCAL_BUILD" -eq 1 ]]; then
  echo "starting stack with local build …"
  "${COMPOSE[@]}" -f docker-compose.yml -f docker-compose.build.yml up -d --build
else
  echo "pulling ${VMCP_IMAGE} and starting stack …"
  if ! docker pull "$VMCP_IMAGE"; then
    die "failed to pull ${VMCP_IMAGE}. For private packages: docker login ghcr.io -u USERNAME. Or use --build."
  fi
  "${COMPOSE[@]}" up -d
fi

echo
echo "stack is up. verify:"
echo "  curl -fsS https://${DOMAIN}/health"
echo "  docker compose exec vmcp print-config | grep master_password"
echo
echo "image: ${VMCP_IMAGE}  (from .github/workflows/release.yml on v* tags)"
