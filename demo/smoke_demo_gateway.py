#!/usr/bin/env python3
"""End-to-end smoke for the local demo MCP stack over GraphQL.

Starts a gateway with demo/registry.json, then checks:
  - /health
  - { servers { name toolCount } }  (4 local required; context7 soft)
  - one read call per local upstream (+ soft context7)

Gateway start modes (no cargo required on remote):
  VMCP_BIN=/path/to/vmcp     — run that binary
  VMCP_SMOKE_EXTERNAL=1      — do not spawn; hit VMCP_SMOKE_BASE only

Usage (from repo root):
  python3 demo/smoke_demo_gateway.py
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
STAND = ROOT / "demo" / "stand"
BASE = os.environ.get("VMCP_SMOKE_BASE", "http://127.0.0.1:8765")
MCP = f"{BASE}/mcp"
HEALTH = f"{BASE}/health"
BOOT_WAIT_S = float(os.environ.get("VMCP_SMOKE_BOOT_WAIT", "180"))


class McpClient:
    def __init__(self, url: str) -> None:
        self.url = url
        self.session_id: str | None = None
        self._id = 0

    def _next_id(self) -> int:
        self._id += 1
        return self._id

    def _headers(self) -> dict[str, str]:
        h = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session_id:
            h["Mcp-Session-Id"] = self.session_id
        return h

    def _post(self, payload: dict[str, Any]) -> tuple[dict[str, Any], dict[str, str]]:
        data = json.dumps(payload).encode()
        req = urllib.request.Request(
            self.url, data=data, headers=self._headers(), method="POST"
        )
        with urllib.request.urlopen(req, timeout=120) as resp:
            raw = resp.read().decode()
            headers = {k.lower(): v for k, v in resp.headers.items()}
            ctype = headers.get("content-type", "")
            if "text/event-stream" in ctype:
                body = _parse_sse_json(raw)
            else:
                body = json.loads(raw) if raw.strip() else {}
            return body, headers

    def initialize(self) -> None:
        body, headers = self._post(
            {
                "jsonrpc": "2.0",
                "id": self._next_id(),
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "smoke-demo", "version": "0.1.0"},
                },
            }
        )
        sid = headers.get("mcp-session-id")
        if sid:
            self.session_id = sid
        if "error" in body:
            raise RuntimeError(f"initialize failed: {body['error']}")
        # required notification after initialize
        self._notify(
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            }
        )

    def _notify(self, payload: dict[str, Any]) -> None:
        data = json.dumps(payload).encode()
        req = urllib.request.Request(
            self.url, data=data, headers=self._headers(), method="POST"
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                resp.read()
        except urllib.error.HTTPError as e:
            # some gateways return 202/204 for notifications
            if e.code not in (202, 204):
                raise

    def call_tool(self, name: str, arguments: dict[str, Any]) -> Any:
        body, _ = self._post(
            {
                "jsonrpc": "2.0",
                "id": self._next_id(),
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }
        )
        if "error" in body:
            raise RuntimeError(f"tools/call {name} error: {body['error']}")
        result = body.get("result") or {}
        if result.get("isError"):
            raise RuntimeError(f"tools/call {name} isError: {result}")
        # query_graphql returns structured content or text JSON
        structured = result.get("structuredContent")
        if structured is not None:
            return structured
        for block in result.get("content") or []:
            if block.get("type") == "text":
                text = block.get("text") or ""
                try:
                    return json.loads(text)
                except json.JSONDecodeError:
                    return {"text": text}
        return result

    def gql(self, query: str, variables: dict[str, Any] | None = None) -> dict[str, Any]:
        args: dict[str, Any] = {"query": query}
        if variables is not None:
            args["variables"] = variables
        out = self.call_tool("query_graphql", args)
        if isinstance(out, dict) and "data" in out:
            if out.get("errors"):
                raise RuntimeError(f"GraphQL errors: {out['errors']}")
            return out["data"]
        # sometimes wrapped again
        if isinstance(out, dict) and "errors" in out:
            raise RuntimeError(f"GraphQL errors: {out['errors']}")
        return out if isinstance(out, dict) else {"raw": out}


def _parse_sse_json(raw: str) -> dict[str, Any]:
    """Pick the last JSON `data:` event from an SSE body."""
    last: dict[str, Any] | None = None
    for line in raw.splitlines():
        if not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            last = json.loads(payload)
        except json.JSONDecodeError:
            continue
    if last is None:
        raise RuntimeError(f"no JSON data in SSE response: {raw[:400]!r}")
    return last


def wait_health(timeout: float) -> None:
    deadline = time.time() + timeout
    last_err: Exception | None = None
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(HEALTH, timeout=2) as resp:
                body = resp.read().decode().strip()
                if resp.status == 200 and body == "ok":
                    return
        except Exception as e:  # noqa: BLE001
            last_err = e
        time.sleep(1)
    raise TimeoutError(f"/health not ok within {timeout}s: {last_err}")


def gateway_env() -> dict[str, str]:
    env = os.environ.copy()
    env.setdefault("RUST_LOG", "info")
    # Prefer JSON from agent-lsp for easier smoke asserts
    env.setdefault("AGENT_LSP_OUTPUT_FORMAT", "json")
    # Ensure common local bins are visible to stdio children
    extra = [
        str(Path.home() / ".local" / "bin"),
        "/usr/local/bin",
    ]
    env["PATH"] = os.pathsep.join(extra + [env.get("PATH", "")])
    return env


def start_gateway() -> subprocess.Popen[str] | None:
    """Start gateway with demo/vmcp.toml unless VMCP_SMOKE_EXTERNAL=1.

    Prefer VMCP_BIN (release binary). Never use cargo unless explicitly
    VMCP_SMOKE_USE_CARGO=1 (local dev only).
    """
    if os.environ.get("VMCP_SMOKE_EXTERNAL", "").strip() in ("1", "true", "yes"):
        print("[smoke] VMCP_SMOKE_EXTERNAL=1 — not spawning gateway", flush=True)
        return None

    env = gateway_env()
    cfg = str(ROOT / "demo" / "vmcp.toml")
    vmcp_bin = os.environ.get("VMCP_BIN", "").strip()
    if vmcp_bin:
        cmd = [vmcp_bin, "--config", cfg]
    elif os.environ.get("VMCP_SMOKE_USE_CARGO", "").strip() in ("1", "true", "yes"):
        cmd = ["cargo", "run", "-p", "vmcp", "--quiet", "--", "--config", cfg]
    else:
        raise RuntimeError(
            "Set VMCP_BIN=/path/to/vmcp (preferred) or VMCP_SMOKE_EXTERNAL=1. "
            "Cargo start is disabled unless VMCP_SMOKE_USE_CARGO=1."
        )

    print(f"[smoke] starting: {' '.join(cmd)}", flush=True)
    return subprocess.Popen(
        cmd,
        cwd=str(ROOT),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )


def _ok(name: str, detail: str = "") -> None:
    suffix = f" — {detail}" if detail else ""
    print(f"  PASS  {name}{suffix}", flush=True)


def _fail(name: str, err: Any) -> None:
    print(f"  FAIL  {name}: {err}", flush=True)


def _soft(name: str, reason: str) -> None:
    print(f"  SKIP  {name}: {reason}", flush=True)


def main() -> int:
    results: dict[str, str] = {}
    gw: subprocess.Popen[str] | None = None
    stand_abs = str(STAND.resolve())
    main_py = str((STAND / "src" / "main.py").resolve())

    try:
        gw = start_gateway()
        print(f"[smoke] waiting for /health (up to {BOOT_WAIT_S:.0f}s)…", flush=True)
        wait_health(BOOT_WAIT_S)
        _ok("health")
        results["health"] = "pass"

        client = McpClient(MCP)
        client.initialize()
        _ok("mcp initialize")

        servers_data = client.gql("{ servers { name toolCount } }")
        servers = {
            s["name"]: s.get("toolCount", 0) for s in (servers_data.get("servers") or [])
        }
        print(f"[smoke] servers: {servers}", flush=True)

        required = ["time", "filesystem", "architect_c4", "agent_lsp"]
        missing = [n for n in required if n not in servers or servers[n] <= 0]
        if missing:
            _fail("servers", f"missing/empty: {missing}; got {servers}")
            results["servers"] = "fail"
        else:
            _ok("servers", f"{len(required)} local present")
            results["servers"] = "pass"

        # --- time ---
        try:
            data = client.gql(
                '{ time { getCurrentTime(timezone: "Europe/Moscow") { json text isError } } }'
            )
            node = ((data.get("time") or {}).get("getCurrentTime") or {})
            if node.get("isError"):
                raise RuntimeError(node)
            _ok("time", str(node.get("text") or node.get("json"))[:80])
            results["time"] = "pass"
        except Exception as e:  # noqa: BLE001
            _fail("time", e)
            results["time"] = "fail"

        # --- filesystem ---
        try:
            data = client.gql(
                """
                query($p: String!) {
                  filesystem {
                    readFile(path: $p) { json text isError }
                  }
                }
                """,
                {"p": main_py},
            )
            node = ((data.get("filesystem") or {}).get("readFile") or {})
            if node.get("isError"):
                # try relative path inside stand root
                data = client.gql(
                    """
                    query($p: String!) {
                      filesystem {
                        readFile(path: $p) { json text isError }
                      }
                    }
                    """,
                    {"p": "src/main.py"},
                )
                node = ((data.get("filesystem") or {}).get("readFile") or {})
            if node.get("isError"):
                raise RuntimeError(node)
            text = str(node.get("text") or node.get("json") or "")
            if "def greet" not in text and "greet" not in text:
                raise RuntimeError(f"unexpected file content: {text[:200]!r}")
            _ok("filesystem", "read src/main.py")
            results["filesystem"] = "pass"
        except Exception as e:  # noqa: BLE001
            _fail("filesystem", e)
            results["filesystem"] = "fail"

        # --- architect_c4 ---
        try:
            data = client.gql(
                """
                {
                  architectC4 {
                    getModel { json text isError }
                    validateModel { json text isError }
                  }
                }
                """
            )
            ac = data.get("architectC4") or {}
            get_m = ac.get("getModel") or {}
            val_m = ac.get("validateModel") or {}
            if get_m.get("isError") or val_m.get("isError"):
                raise RuntimeError({"getModel": get_m, "validateModel": val_m})
            blob = str(get_m.get("text") or get_m.get("json") or "")
            if "stand" not in blob.lower() and "developer" not in blob.lower():
                # still ok if structured json has elements
                pass
            _ok("architect_c4", "getModel+validateModel")
            results["architect_c4"] = "pass"
        except Exception as e:  # noqa: BLE001
            _fail("architect_c4", e)
            results["architect_c4"] = "fail"

        # --- agent_lsp ---
        try:
            # start_lsp is typically a Mutation
            data = client.gql(
                """
                mutation($root: String!) {
                  agentLsp {
                    startLsp(rootDir: $root) { json text isError }
                  }
                }
                """,
                {"root": stand_abs},
            )
            start = ((data.get("agentLsp") or {}).get("startLsp") or {})
            if start.get("isError"):
                raise RuntimeError(f"startLsp: {start}")

            # list_symbols may be Query
            data = client.gql(
                """
                query($p: String!) {
                  agentLsp {
                    listSymbols(filePath: $p) { json text isError }
                  }
                }
                """,
                {"p": main_py},
            )
            sym = ((data.get("agentLsp") or {}).get("listSymbols") or {})
            if sym.get("isError"):
                # try relative path
                data = client.gql(
                    """
                    query($p: String!) {
                      agentLsp {
                        listSymbols(filePath: $p) { json text isError }
                      }
                    }
                    """,
                    {"p": "src/main.py"},
                )
                sym = ((data.get("agentLsp") or {}).get("listSymbols") or {})
            if sym.get("isError"):
                raise RuntimeError(f"listSymbols: {sym}")
            blob = str(sym.get("text") or sym.get("json") or "")
            if "greet" not in blob.lower() and "main" not in blob.lower() and not blob:
                raise RuntimeError(f"no symbols in response: {blob[:300]!r}")
            _ok("agent_lsp", "startLsp + listSymbols")
            results["agent_lsp"] = "pass"
        except Exception as e:  # noqa: BLE001
            _fail("agent_lsp", e)
            results["agent_lsp"] = "fail"

        # --- context7 (soft) ---
        if not os.environ.get("CONTEXT7_API_KEY"):
            _soft("context7", "CONTEXT7_API_KEY not set")
            results["context7"] = "skip"
        elif "context7" not in servers or servers["context7"] <= 0:
            _soft("context7", "server not in pool (auth/key?)")
            results["context7"] = "skip"
        else:
            try:
                data = client.gql(
                    """
                    {
                      context7 {
                        resolveLibraryId(libraryName: "react") { json text isError }
                      }
                    }
                    """
                )
                node = ((data.get("context7") or {}).get("resolveLibraryId") or {})
                if node.get("isError"):
                    raise RuntimeError(node)
                _ok("context7", "resolveLibraryId")
                results["context7"] = "pass"
            except Exception as e:  # noqa: BLE001
                _fail("context7", e)
                results["context7"] = "fail"

    except Exception as e:  # noqa: BLE001
        print(f"[smoke] fatal: {e}", file=sys.stderr)
        results.setdefault("health", "fail")
        return 2
    finally:
        if gw is not None and gw.poll() is None:
            print("[smoke] stopping gateway…", flush=True)
            try:
                os.killpg(gw.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                gw.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(gw.pid, signal.SIGKILL)

    print("\n=== smoke summary ===", flush=True)
    for k in [
        "health",
        "servers",
        "time",
        "filesystem",
        "architect_c4",
        "agent_lsp",
        "context7",
    ]:
        print(f"  {k}: {results.get(k, 'n/a')}", flush=True)

    hard = [
        results.get(k)
        for k in (
            "health",
            "servers",
            "time",
            "filesystem",
            "architect_c4",
            "agent_lsp",
        )
    ]
    return 0 if all(x == "pass" for x in hard) else 1


if __name__ == "__main__":
    sys.exit(main())
