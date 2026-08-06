#!/usr/bin/env bash
# Leak check for vmcp-auth.
#
# 1) Static: AuthState::{codes,consents} GC must be scheduled from serve_http.
# 2) Dynamic: nightly AddressSanitizer + LeakSanitizer on the lib test suite.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="${LEAK_TARGET:-x86_64-unknown-linux-gnu}"
OUT_DIR="${LEAK_TARGET_DIR:-/tmp/vmcp-leak-check}"
FILTER="${1:-}"

echo "== [1/2] static: auth GC wiring =="
rg -n "AUTH_EPHEMERAL_MAX_AGE" crates/vmcp-auth/src/state.rs >/dev/null
rg -n 'auth\.gc\(vmcp_auth::AUTH_EPHEMERAL_MAX_AGE\)' crates/vmcp/src/main.rs \
  || { echo "FAIL: serve_http must call AuthState::gc(AUTH_EPHEMERAL_MAX_AGE)"; exit 1; }
echo "OK: AUTH_EPHEMERAL_MAX_AGE + background GC present"

echo "== [2/2] dynamic: ASan/LSan (nightly, build-std) =="
rustup toolchain install nightly --profile minimal -c rust-src >/dev/null
export RUSTFLAGS="-Z sanitizer=address"
# halt_on_error=1 fails the process on the first sanitizer error / leak summary.
export ASAN_OPTIONS="detect_leaks=1:halt_on_error=1"
export CARGO_TARGET_DIR="$OUT_DIR"

cmd=(
  cargo +nightly test -Z build-std
  --target "$TARGET"
  -p vmcp-auth --lib
)
if [[ -n "$FILTER" ]]; then
  cmd+=("$FILTER")
fi
cmd+=(-- --test-threads=1)

echo "+ CARGO_TARGET_DIR=$OUT_DIR RUSTFLAGS=$RUSTFLAGS ${cmd[*]}"
"${cmd[@]}"
echo "OK: vmcp-auth lib tests passed under AddressSanitizer/LeakSanitizer"
