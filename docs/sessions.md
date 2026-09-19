# Session registry and recordings

**Language:** English | [Русский](ru/sessions.md)

The gateway's HTTP sessions (the **Sessions** tab in the admin UI) are stored
as JSON files on disk. The session list therefore survives gateway restarts:
it is loaded from disk again.

Configuration: the `[recorder]` section in [`vmcp.toml`](../vmcp.toml).

```toml
[recorder]
sessions_dir     = "./sessions"   # created automatically
redact_keys      = ["password","secret","token","api_key","Authorization"]
idle_ttl_secs    = 300            # close idle entries + tear down rmcp sessions (FD/SSE)
gc_interval_secs = 30
```

Environment variables:
- `VMCP_RECORDER__SESSIONS_DIR=/var/lib/vmcp/sessions`
- `VMCP_SESSION_CHANNEL_CAPACITY=<n>` — the mpsc window for each Streamable
  HTTP session (uses the rmcp default when unset)

Idle GC not only marks the registry entry as `closed`, but also calls
`LocalSessionManager::close_session` on `/mcp` and `/mcp-proxy`. This prevents
transport workers and their descriptors from accumulating after a storm of
session recreation. `idle_ttl_secs` also sets the rmcp `keep_alive` value.

SSE GET / resume requests with `Mcp-Session-Id` call `registry.touch` (without
incrementing `request_count`) so idle GC does not terminate a live stream
while rmcp `keep_alive` is still being reset by transport events.

---

## On-disk layout

```text
sessions/                          # recorder.sessions_dir
  .registry/
    <session_id>.json              # SessionRegistry (survives restarts)
  <client_id>/
    <session_id>.jsonl             # JSON-RPC / MCP exchange dump
    <session_id>.meta.json         # dump metadata for merging in the admin UI
```

| Path | Role |
| ---- | ---- |
| `.registry/{id}.json` | Live registry entry: client, counters, `active` / `closed` |
| `{client}/{id}.jsonl` | Append-only exchange record (sensitive keys are redacted) |
| `{client}/{id}.meta.json` | Recording summary (`started_at`, `request_count`, `upstream`, …) |

At startup, `SessionRegistry::open(sessions_dir)` loads every
`.registry/*.json` file. `record_request` / `close` / idle GC atomically
rewrite the corresponding file (`.json.tmp` → rename).

Corrupt or unreadable JSON files and files with invalid IDs are skipped with
a warning.

---

## What survives a restart

| Data | Survives? |
| ---- | --------- |
| Registry entries (`.registry/`) | **Yes** — reloaded; status is retained until idle GC runs |
| Exchange recordings (`.jsonl` / `.meta.json`) | **Yes** |
| DCR OAuth `client_id` + unique `name` | **Yes** — SQLite `auth.clients_db_path` ([authentication.md](authentication.md#dcr-clients-survive-restart)) |
| Active MCP transport / rmcp session | **No** — the client must reconnect |
| OAuth JWT access tokens | **No** — consent is required again; alternatively, use static `pre-reg` tokens |

Stale recordings are cleaned up at startup. If a process writing an exchange
recording crashes in the middle of a session, its meta file (.meta.json)
remains in the active state even though recording stopped long ago. On every
startup, the recorder therefore runs startup_cleanup: it finds these stale
meta files and moves them to the closed state. This procedure operates only
on recordings and does not affect .registry/ entries.

---

## Admin UI

`GET /admin/api/sessions` combines:

1. DCR / pre-reg clients (each with a unique operator-facing `name`)
2. A live `SessionRegistry` snapshot (from `.registry/`)
3. Recording meta files from client subdirectories on disk

A client can be renamed in the Sessions column (using its input field) or
through `PATCH /admin/api/sessions/:client_id` with the body `{"name":"…"}`.

Mount `sessions_dir` as a **writable** Docker volume so the registry and
recordings survive container recreation.

---

## Coverage

`sessions.rs` is included in the vmcp-server llvm-cov gate together with
`skills.rs`, `tasks.rs`, and the prompt aggregation modules (the line coverage
threshold is **96%**):

```bash
cargo llvm-cov -p vmcp-server --lib --fail-under-lines 96 \
  --ignore-filename-regex '(^|/)(otel_file|proxy|lib|recorder)\.rs$'
```

Unit tests: `cargo test -p vmcp-server --lib sessions::`.

See also [skills.md](skills.md#tests--coverage),
[clients.md](clients.md#admin-ui), [deployment.md](deployment.md), and
[builds-and-modes.md](builds-and-modes.md).
