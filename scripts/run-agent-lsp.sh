#!/usr/bin/env bash
# Run agent-lsp (MCP over HTTP) for this vmcp workspace with rust-analyzer.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${AGENT_LSP_PORT:-8766}"
BIN="${AGENT_LSP_BIN:-}"
if [[ -z "$BIN" ]]; then
  if command -v agent-lsp >/dev/null 2>&1; then
    BIN="$(command -v agent-lsp)"
  elif [[ -x "$HOME/.local/lib/python3.12/site-packages/agent_lsp/bin/agent-lsp" ]]; then
    BIN="$HOME/.local/lib/python3.12/site-packages/agent_lsp/bin/agent-lsp"
  else
    echo "Install: pip install agent-lsp && rustup component add rust-analyzer" >&2
    exit 1
  fi
fi
mkdir -p /tmp/agent-lsp
exec "$BIN" --http --port "$PORT" --no-auth "rust:rust-analyzer"
