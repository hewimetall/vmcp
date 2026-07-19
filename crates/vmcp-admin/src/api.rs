//! JSON API for the admin panel.
//!
//! All routes return `application/json` (except schema SDL and dump downloads).
//! The SPA in `static/admin.js` consumes these endpoints. The same data drives
//! programmatic clients (curl, scripts, exporters).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use vmcp_auth::RenameClientError;
#[cfg(feature = "otel")]
use vmcp_server::otel_file::StoredSpan;
use vmcp_server::recorder::{McpExchange, SessionKey};
use vmcp_server::sessions::SessionStatus;
use vmcp_server::{
    delete_skill as fs_delete_skill, load_skills, render_skill as render_skill_template,
    save_skill, Skill, SkillArg,
};

use crate::AdminState;

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/servers", get(list_servers))
        .route("/api/servers/:name", get(get_server))
        .route("/api/servers/:name/tools", get(get_server_tools))
        .route("/api/schema.graphql", get(get_schema_sdl))
        .route("/api/skills", get(list_skills).post(create_skill))
        .route(
            "/api/skills/:name",
            get(get_skill)
                .put(update_skill)
                .delete(delete_skill_handler),
        )
        .route("/api/skills/:name/generate", post(render_skill_handler))
        .route("/api/notifications", get(get_notifications))
        // Sessions API — sibling agent C.
        .route("/api/sessions", get(list_sessions))
        .route(
            "/api/sessions/:client_id",
            get(get_session_detail).patch(rename_session_client),
        )
        .route("/api/sessions/:client_id/dump", get(dump_handler))
        .route("/api/sessions/:client_id/dump/stream", get(dump_stream))
        .route("/api/sessions/:client_id/dump.jsonl", get(dump_download))
}

async fn list_servers(State(s): State<AdminState>) -> Json<Value> {
    let items: Vec<Value> = s
        .pool
        .all_resolved()
        .into_iter()
        .map(|(name, tools)| {
            let read_only_count = tools.iter().filter(|t| t.read_only).count();
            json!({
                "name": name,
                "description": s.pool.description_of(&name),
                "tool_count": tools.len(),
                "read_only_count": read_only_count,
                "connected": true,
            })
        })
        .collect();
    Json(json!({ "servers": items }))
}

async fn get_server(
    Path(name): Path<String>,
    State(s): State<AdminState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let tools = s
        .pool
        .resolved(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("unknown server: {name}")))?;
    let read_only_count = tools.iter().filter(|t| t.read_only).count();
    Ok(Json(json!({
        "name": name,
        "description": s.pool.description_of(&name),
        "tool_count": tools.len(),
        "read_only_count": read_only_count,
        "connected": true,
    })))
}

async fn get_server_tools(
    Path(name): Path<String>,
    State(s): State<AdminState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let tools = s
        .pool
        .resolved(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("unknown server: {name}")))?;
    let tools_json: Vec<Value> = tools
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "readOnly": t.read_only,
                "taskSupport": t.task_support.as_str(),
                "inputSchema": t.input_schema,
            })
        })
        .collect();
    Ok(Json(json!({ "server": name, "tools": tools_json })))
}

async fn get_schema_sdl(State(s): State<AdminState>) -> impl IntoResponse {
    let sdl = s.schema.load().sdl();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        sdl,
    )
}

async fn list_skills(State(s): State<AdminState>) -> Json<Value> {
    let snapshot = s.skills.load();
    let skills: Vec<Value> = snapshot
        .iter()
        .map(|sk| {
            let args: Vec<Value> = sk
                .arguments
                .iter()
                .map(|a| {
                    json!({
                        "name": a.name,
                        "description": a.description,
                        "required": a.required,
                        "default": a.default,
                    })
                })
                .collect();
            json!({
                "name": sk.name,
                "description": sk.description,
                "arguments": args,
                "template_preview": preview(&sk.template),
            })
        })
        .collect();
    Json(json!({ "skills": skills }))
}

/// GET /api/skills/:name — return the full Skill JSON or 404.
async fn get_skill(
    Path(name): Path<String>,
    State(s): State<AdminState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let snapshot = s.skills.load();
    let sk = snapshot
        .iter()
        .find(|sk| sk.name == name)
        .ok_or_else(|| not_found(&name))?;
    Ok(Json(skill_to_full_json(sk)))
}

/// Wire-shape mirror of `Skill`. Kept as a parallel struct so the admin API
/// surface is decoupled from any future internal-only fields on `Skill`.
#[derive(Debug, Deserialize)]
struct SkillBody {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub arguments: Vec<SkillArgBody>,
    pub template: String,
}

#[derive(Debug, Deserialize)]
struct SkillArgBody {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

impl From<SkillBody> for Skill {
    fn from(b: SkillBody) -> Self {
        Skill {
            name: b.name,
            description: b.description,
            arguments: b
                .arguments
                .into_iter()
                .map(|a| SkillArg {
                    name: a.name,
                    description: a.description,
                    required: a.required,
                    default: a.default,
                })
                .collect(),
            template: b.template,
        }
    }
}

/// POST /api/skills — create a new skill. 409 if the name is taken, 400 on
/// validation failure.
async fn create_skill(
    State(s): State<AdminState>,
    Json(body): Json<SkillBody>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let _lock = s.skills_write_lock.lock().await;

    let skill: Skill = body.into();

    // Conflict check against the current snapshot.
    {
        let snapshot = s.skills.load();
        if snapshot.iter().any(|sk| sk.name == skill.name) {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("skill `{}` already exists", skill.name) })),
            ));
        }
    }

    if let Err(e) = save_skill(&s.skills_dir, &skill) {
        return Err(bad_request(e.to_string()));
    }

    reload_and_swap(&s).map_err(|e| internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(json!({ "name": skill.name }))))
}

/// PUT /api/skills/:name — replace an existing skill. 400 on name mismatch,
/// 404 if not currently loaded, 400 on validation failure.
async fn update_skill(
    Path(name): Path<String>,
    State(s): State<AdminState>,
    Json(body): Json<SkillBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let _lock = s.skills_write_lock.lock().await;

    if name != body.name {
        return Err(bad_request(format!(
            "path name `{}` does not match body name `{}`",
            name, body.name
        )));
    }

    {
        let snapshot = s.skills.load();
        if !snapshot.iter().any(|sk| sk.name == name) {
            return Err(not_found(&name));
        }
    }

    let skill: Skill = body.into();
    if let Err(e) = save_skill(&s.skills_dir, &skill) {
        return Err(bad_request(e.to_string()));
    }

    reload_and_swap(&s).map_err(|e| internal(e.to_string()))?;

    Ok(Json(json!({ "name": skill.name })))
}

/// DELETE /api/skills/:name — remove a skill. 404 if not present.
async fn delete_skill_handler(
    Path(name): Path<String>,
    State(s): State<AdminState>,
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    let _lock = s.skills_write_lock.lock().await;

    {
        let snapshot = s.skills.load();
        if !snapshot.iter().any(|sk| sk.name == name) {
            return Err(not_found(&name));
        }
    }

    if let Err(e) = fs_delete_skill(&s.skills_dir, &name) {
        return Err(bad_request(e.to_string()));
    }

    reload_and_swap(&s).map_err(|e| internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct RenderRequest {
    #[serde(default)]
    pub args: HashMap<String, String>,
}

/// POST /api/skills/:name/generate — render a skill's template with the given
/// args, returning `{"rendered": "..."}`. 404 if the skill is unknown, 400 if
/// the render fails (missing required arg, etc).
async fn render_skill_handler(
    Path(name): Path<String>,
    State(s): State<AdminState>,
    Json(req): Json<RenderRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let skill = {
        let snapshot = s.skills.load();
        snapshot
            .iter()
            .find(|sk| sk.name == name)
            .cloned()
            .ok_or_else(|| not_found(&name))?
    };
    let rendered =
        render_skill_template(&skill, &req.args).map_err(|e| bad_request(e.to_string()))?;
    Ok(Json(json!({ "rendered": rendered })))
}

/// Reload `skills_dir` from disk and `.store()` the result into the shared
/// ArcSwap. Called by every mutating handler after a successful fs op.
fn reload_and_swap(s: &AdminState) -> anyhow::Result<()> {
    let reloaded = load_skills(&s.skills_dir)?;
    s.skills.store(Arc::new(reloaded));
    Ok(())
}

fn skill_to_full_json(sk: &Skill) -> Value {
    let args: Vec<Value> = sk
        .arguments
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "description": a.description,
                "required": a.required,
                "default": a.default,
            })
        })
        .collect();
    json!({
        "name": sk.name,
        "description": sk.description,
        "arguments": args,
        "template": sk.template,
    })
}

fn not_found(name: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("unknown skill: {name}") })),
    )
}

fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.into() })),
    )
}

fn internal(msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg.into() })),
    )
}

#[derive(Debug, Deserialize)]
struct NotifQuery {
    #[serde(default)]
    since: u64,
    #[serde(default = "default_notif_limit")]
    limit: usize,
}

fn default_notif_limit() -> usize {
    100
}

async fn get_notifications(
    State(s): State<AdminState>,
    Query(q): Query<NotifQuery>,
) -> Json<Value> {
    let items: Vec<Value> = s
        .bus
        .replay_since(q.since, q.limit)
        .into_iter()
        .map(|n| {
            json!({
                "id": n.id,
                "source": n.source,
                "method": n.method,
                "params": n.params,
                "ts_unix_ms": n.ts_unix_ms,
            })
        })
        .collect();
    Json(json!({ "notifications": items, "next_id_hint": s.bus.next_id_hint() }))
}

/// Trim a skill template body for the admin UI list view (first 200 chars).
fn preview(template: &str) -> String {
    if template.len() <= 200 {
        template.to_string()
    } else {
        let mut out: String = template.chars().take(200).collect();
        out.push('…');
        out
    }
}

// ---------- Sessions API (part C) ----------

/// Validate that an identifier matches `^[A-Za-z0-9_-]{1,max}$`. Used to
/// gate every client_id / session_id path parameter so we never resolve a
/// recorder path that contains `..`, `/`, or shell metacharacters.
fn validate_id(s: &str, max: usize) -> bool {
    if s.is_empty() || s.len() > max {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_client_id(s: &str) -> bool {
    // Matches DCR-generated `vmcp-<uuid-ish>` (~37 chars) but also any
    // alnum-dashes-underscores identifier up to 128 chars.
    validate_id(s, 128)
}

fn validate_session_id(s: &str) -> bool {
    validate_id(s, 128)
}

async fn list_sessions(State(s): State<AdminState>) -> Json<Value> {
    let registered: Vec<vmcp_auth::types::ClientInfo> = s.auth_state.list_clients();
    let live = s.registry.snapshot();
    let disk = s
        .recorder
        .list_all_clients_with_meta()
        .await
        .unwrap_or_default();
    Json(json!({ "clients": merge_session_clients(&registered, &live, &disk) }))
}

/// Pure merge of DCR clients + live registry + on-disk metas. Extracted so the
/// branchy aggregation can be unit-tested without spinning an Axum router.
fn merge_session_clients(
    registered: &[vmcp_auth::types::ClientInfo],
    live: &[vmcp_server::sessions::SessionSnapshot],
    disk: &[(String, Vec<vmcp_server::recorder::SessionMeta>)],
) -> Vec<Value> {
    let mut clients: std::collections::HashMap<String, Value> = Default::default();
    for c in registered {
        clients.insert(
            c.client_id.clone(),
            json!({
                "client_id": c.client_id,
                "name": c.name,
                "client_name": c.client_name,
                "state": "pre_registered",
                "registered_at_ms": c.issued_at.timestamp_millis(),
                "redirect_uris": c.redirect_uris,
                "scope": c.scope,
                "sessions": [],
            }),
        );
    }
    for ss in live {
        let cid = ss.client_id.clone().unwrap_or_else(|| "unknown".into());
        let entry = clients.entry(cid.clone()).or_insert_with(|| {
            json!({
                "client_id": cid,
                "name": null,
                "client_name": ss.client_name,
                "state": "active",
                "registered_at_ms": ss.started_at_ms,
                "redirect_uris": [],
                "scope": null,
                "sessions": [],
            })
        });
        let sess = json!({
            "id": ss.id,
            "started_at_ms": ss.started_at_ms,
            "last_seen_ms": ss.last_seen_ms,
            "request_count": ss.request_count,
            "duration_ms": ss.duration_ms,
            "status": match ss.status {
                SessionStatus::Active => "active",
                SessionStatus::Closed => "closed",
            },
        });
        if let Some(arr) = entry.get_mut("sessions").and_then(|v| v.as_array_mut()) {
            arr.push(sess);
        }
        if ss.status == SessionStatus::Active {
            if let Some(state) = entry.get_mut("state") {
                *state = json!("active");
            }
        } else if entry.get("state").and_then(|v| v.as_str()) == Some("pre_registered") {
            if let Some(state) = entry.get_mut("state") {
                *state = json!("idle");
            }
        }
    }
    for (cid, metas) in disk {
        let entry = clients.entry(cid.clone()).or_insert_with(|| {
            json!({
                "client_id": cid,
                "name": null,
                "client_name": metas.first().and_then(|m| m.client_name.clone()),
                "state": "idle",
                "registered_at_ms": metas.first().map(|m| m.started_at_ms).unwrap_or(0),
                "redirect_uris": [],
                "scope": null,
                "sessions": [],
            })
        });
        for m in metas {
            let live_idx = entry
                .get("sessions")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter().position(|j| {
                        j.get("id").and_then(|v| v.as_str()) == Some(m.session_id.as_str())
                    })
                })
                .unwrap_or(None);
            if let Some(idx) = live_idx {
                if let Some(arr) = entry.get_mut("sessions").and_then(|v| v.as_array_mut()) {
                    if let Some(row) = arr.get_mut(idx) {
                        if row.get("upstream").map(|v| v.is_null()).unwrap_or(true) {
                            if let Some(obj) = row.as_object_mut() {
                                obj.insert("upstream".into(), json!(m.upstream));
                            }
                        }
                    }
                }
                continue;
            }
            if let Some(arr) = entry.get_mut("sessions").and_then(|v| v.as_array_mut()) {
                arr.push(json!({
                    "id": m.session_id,
                    "started_at_ms": m.started_at_ms,
                    "last_seen_ms": m.ended_at_ms.unwrap_or(m.started_at_ms),
                    "request_count": m.request_count,
                    "duration_ms": m.ended_at_ms
                        .map(|e| e - m.started_at_ms)
                        .unwrap_or(0),
                    "status": m.status,
                    "upstream": m.upstream,
                }));
            }
        }
    }
    let mut out: Vec<Value> = clients.into_values().collect();
    out.sort_by_key(|c| match c.get("state").and_then(|v| v.as_str()) {
        Some("active") => 0,
        Some("pre_registered") => 1,
        _ => 2,
    });
    out
}

async fn get_session_detail(
    Path(client_id): Path<String>,
    State(s): State<AdminState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !validate_client_id(&client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client_id".into()));
    }
    let info = s
        .auth_state
        .list_clients()
        .into_iter()
        .find(|c| c.client_id == client_id);
    let live: Vec<_> = s
        .registry
        .snapshot()
        .into_iter()
        .filter(|ss| ss.client_id.as_deref() == Some(client_id.as_str()))
        .collect();
    let disk = s
        .recorder
        .list_client_sessions(&client_id)
        .await
        .unwrap_or_default();
    if info.is_none() && live.is_empty() && disk.is_empty() {
        return Err((StatusCode::NOT_FOUND, "client not found".into()));
    }
    Ok(Json(json!({
        "client": info.map(|c| json!({
            "client_id": c.client_id,
            "name": c.name,
            "client_name": c.client_name,
            "redirect_uris": c.redirect_uris,
            "scope": c.scope,
            "registered_at_ms": c.issued_at.timestamp_millis(),
        })),
        "live_sessions": live,
        "historical_sessions": disk,
    })))
}

#[derive(Debug, Deserialize)]
struct RenameClientBody {
    name: String,
}

/// PATCH /api/sessions/:client_id — rename a DCR client's operator `name`.
async fn rename_session_client(
    Path(client_id): Path<String>,
    State(s): State<AdminState>,
    Json(body): Json<RenameClientBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !validate_client_id(&client_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid client_id" })),
        ));
    }
    let name = body.name.trim().to_string();
    match s.auth_state.rename_client(&client_id, &name) {
        Ok(info) => Ok(Json(json!({
            "client_id": info.client_id,
            "name": info.name,
            "client_name": info.client_name,
        }))),
        Err(e) => Err(map_rename_error(e)),
    }
}

/// Map [`RenameClientError`] → HTTP status + JSON body. Pure so unit tests can
/// hit every arm (including rare `Store` races) without a live SQLite store.
fn map_rename_error(e: RenameClientError) -> (StatusCode, Json<Value>) {
    use vmcp_auth::client_store::ClientStoreError;
    match e {
        RenameClientError::InvalidName(n) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "invalid name `{n}` — use 1..=64 chars of [a-z0-9_-]"
                )
            })),
        ),
        RenameClientError::NameTaken(n) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("name `{n}` already taken") })),
        ),
        RenameClientError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "client not found" })),
        ),
        RenameClientError::Store(ClientStoreError::NameTaken(n)) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("name `{n}` already taken") })),
        ),
        RenameClientError::Store(ClientStoreError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "client not found" })),
        ),
        RenameClientError::Store(ClientStoreError::InvalidName(n)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "invalid name `{n}` — use 1..=64 chars of [a-z0-9_-]"
                )
            })),
        ),
        RenameClientError::Store(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("persist rename: {e}") })),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct DumpQuery {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default = "default_dump_limit")]
    limit: usize,
    #[serde(default)]
    since_seq: Option<u64>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    method: Option<String>,
}

fn default_dump_limit() -> usize {
    200
}

async fn dump_handler(
    Path(client_id): Path<String>,
    Query(q): Query<DumpQuery>,
    State(s): State<AdminState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if !validate_client_id(&client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client_id".into()));
    }
    if let Some(sid) = &q.session_id {
        if !validate_session_id(sid) {
            return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
        }
    }
    let limit = q.limit.min(1000);
    let dir = s.recorder.root.join(&client_id);
    if !dir.is_dir() {
        let has_client = s
            .auth_state
            .list_clients()
            .iter()
            .any(|c| c.client_id == client_id);
        if !has_client {
            return Err((StatusCode::NOT_FOUND, "no client and no dump".into()));
        }
        #[cfg(feature = "otel")]
        {
            return Ok(Json(json!({ "spans": [], "next_seq_hint": null })));
        }
        #[cfg(not(feature = "otel"))]
        {
            return Ok(Json(json!({ "exchanges": [], "next_seq_hint": null })));
        }
    }
    let paths: Vec<std::path::PathBuf> = if let Some(sid) = &q.session_id {
        vec![dir.join(format!("{sid}.jsonl"))]
    } else {
        let mut v = vec![];
        if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(ent)) = rd.next_entry().await {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    v.push(p);
                }
            }
        }
        v
    };

    #[cfg(feature = "otel")]
    {
        let mut all: Vec<StoredSpan> = vec![];
        for p in paths {
            all.extend(s.recorder.load_spans(&p).await);
        }
        all.sort_by_key(|e| e.start_time_unix_ms);
        let filtered: Vec<StoredSpan> = all
            .into_iter()
            .filter(|e| {
                q.direction
                    .as_deref()
                    .map(|d| {
                        e.direction()
                            .map(|x| x.eq_ignore_ascii_case(d))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
            })
            .filter(|e| {
                q.method
                    .as_deref()
                    .map(|m| e.method() == Some(m))
                    .unwrap_or(true)
            })
            .take(limit)
            .collect();
        return Ok(Json(json!({ "spans": filtered, "next_seq_hint": null })));
    }

    #[cfg(not(feature = "otel"))]
    {
        let mut all: Vec<McpExchange> = vec![];
        for p in paths {
            if let Ok(content) = tokio::fs::read_to_string(&p).await {
                for line in content.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(ex) = serde_json::from_str::<McpExchange>(line) {
                        all.push(ex);
                    }
                }
            }
        }
        all.sort_by_key(|e| e.seq);
        let filtered: Vec<McpExchange> = all
            .into_iter()
            .filter(|e| q.since_seq.map(|s| e.seq > s).unwrap_or(true))
            .filter(|e| {
                q.direction
                    .as_deref()
                    .map(|d| format!("{:?}", e.direction).eq_ignore_ascii_case(d))
                    .unwrap_or(true)
            })
            .filter(|e| {
                q.method
                    .as_deref()
                    .map(|m| e.method.as_deref() == Some(m))
                    .unwrap_or(true)
            })
            .take(limit)
            .collect();
        let next = filtered.last().map(|e| e.seq + 1);
        Ok(Json(
            json!({ "exchanges": filtered, "next_seq_hint": next }),
        ))
    }
}

async fn dump_stream(
    Path(client_id): Path<String>,
    Query(q): Query<DumpQuery>,
    State(s): State<AdminState>,
) -> Result<
    axum::response::sse::Sse<
        std::pin::Pin<
            Box<
                dyn futures::Stream<
                        Item = Result<axum::response::sse::Event, std::convert::Infallible>,
                    > + Send,
            >,
        >,
    >,
    (StatusCode, String),
> {
    use axum::response::sse::{KeepAlive, Sse};
    use futures::StreamExt;
    use tokio_stream::wrappers::BroadcastStream;

    if !validate_client_id(&client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client_id".into()));
    }
    let session_id = q
        .session_id
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "session_id required".into()))?;
    if !validate_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }

    let key = SessionKey {
        client_id: client_id.clone(),
        session_id: session_id.clone(),
    };
    let receiver = s.recorder.subscribe(&key);
    let stream = BroadcastStream::new(receiver).map(|res| Ok(sse_event_from_broadcast(res)));
    let boxed: std::pin::Pin<
        Box<
            dyn futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
                + Send,
        >,
    > = Box::pin(stream);
    Ok(Sse::new(boxed).keep_alive(KeepAlive::default()))
}

fn sse_event_from_broadcast(
    res: Result<
        std::sync::Arc<McpExchange>,
        tokio_stream::wrappers::errors::BroadcastStreamRecvError,
    >,
) -> axum::response::sse::Event {
    use axum::response::sse::Event;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
    match res {
        Ok(ex) => Event::default().json_data(&*ex).unwrap_or_default(),
        Err(BroadcastStreamRecvError::Lagged(n)) => Event::default()
            .event("lag")
            .data(format!(r#"{{"dropped":{n}}}"#)),
    }
}

async fn dump_download(
    Path(client_id): Path<String>,
    Query(q): Query<DumpQuery>,
    State(s): State<AdminState>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::body::Body;
    use axum::http::header;
    use tokio_util::io::ReaderStream;

    if !validate_client_id(&client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client_id".into()));
    }
    let session_id = q
        .session_id
        .clone()
        .ok_or((StatusCode::BAD_REQUEST, "session_id required".into()))?;
    if !validate_session_id(&session_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid session_id".into()));
    }
    let path = s
        .recorder
        .root
        .join(&client_id)
        .join(format!("{session_id}.jsonl"));
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "no dump".into()))?;
    let body = Body::from_stream(ReaderStream::new(file));
    Ok(axum::response::Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{client_id}__{session_id}.jsonl\""),
        )
        .body(body)
        .unwrap())
}

#[cfg(test)]
mod validation_tests {
    use super::{
        internal, preview, sse_event_from_broadcast, validate_client_id, validate_session_id,
    };
    use axum::http::StatusCode;
    use serde_json::json;
    use std::sync::Arc;
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
    use vmcp_server::recorder::{Direction, Kind, McpExchange};

    #[test]
    fn rejects_path_traversal() {
        assert!(!validate_client_id("../etc"));
        assert!(!validate_client_id("a/b"));
        assert!(!validate_client_id(""));
        assert!(!validate_client_id(&"x".repeat(200)));
        assert!(!validate_client_id("foo bar")); // whitespace
        assert!(!validate_client_id("a;b")); // shell metachar
    }

    #[test]
    fn accepts_valid() {
        assert!(validate_client_id("vmcp-9e8a4f12"));
        assert!(validate_session_id("sess-01HXYZ7K9P2QABCDEF"));
        assert!(validate_client_id("a")); // single char ok
        assert!(validate_client_id("under_score-1"));
    }

    #[test]
    fn internal_helper_is_500() {
        let (st, body) = internal("boom");
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0["error"], "boom");
    }

    #[test]
    fn preview_short_and_long() {
        assert_eq!(preview("short"), "short");
        let long = "x".repeat(250);
        let p = preview(&long);
        assert!(p.ends_with('…'));
        assert!(p.chars().count() <= 201);
    }

    #[test]
    fn sse_event_ok_and_lagged() {
        let ex = Arc::new(McpExchange {
            seq: 1,
            client_id: Some("c".into()),
            session_id: Some("s".into()),
            timestamp_ms: 1,
            direction: Direction::C2S,
            kind: Kind::Request,
            method: Some("ping".into()),
            jsonrpc_id: Some(json!(1)),
            body: json!({}),
            latency_ms: None,
            upstream: None,
        });
        let ok = sse_event_from_broadcast(Ok(ex));
        // Event debug form isn't stable; just ensure construction doesn't panic.
        let _ = ok;
        let lag = sse_event_from_broadcast(Err(BroadcastStreamRecvError::Lagged(3)));
        let _ = lag;
    }

    #[test]
    fn merge_session_clients_covers_all_branches() {
        use super::merge_session_clients;
        use vmcp_auth::types::ClientInfo;
        use vmcp_server::recorder::SessionMeta;
        use vmcp_server::sessions::{SessionSnapshot, SessionStatus};

        let registered = vec![ClientInfo {
            client_id: "reg".into(),
            redirect_uris: vec![],
            client_name: Some("R".into()),
            name: "r".into(),
            grant_types: vec![],
            response_types: vec![],
            scope: None,
            issued_at: chrono::Utc::now(),
        }];
        let live = vec![
            SessionSnapshot {
                id: "live-active".into(),
                client_id: Some("reg".into()),
                client_name: Some("R".into()),
                started_at_ms: 1,
                last_seen_ms: 2,
                request_count: 1,
                duration_ms: 1,
                status: SessionStatus::Active,
            },
            SessionSnapshot {
                id: "live-closed".into(),
                client_id: Some("reg".into()),
                client_name: Some("R".into()),
                started_at_ms: 1,
                last_seen_ms: 2,
                request_count: 1,
                duration_ms: 1,
                status: SessionStatus::Closed,
            },
            SessionSnapshot {
                id: "orphan".into(),
                client_id: None,
                client_name: None,
                started_at_ms: 9,
                last_seen_ms: 9,
                request_count: 0,
                duration_ms: 0,
                status: SessionStatus::Active,
            },
        ];
        // Second registered-only closed path: pre_registered + Closed → idle.
        let registered2 = vec![ClientInfo {
            client_id: "idle-only".into(),
            redirect_uris: vec![],
            client_name: None,
            name: "idle-only".into(),
            grant_types: vec![],
            response_types: vec![],
            scope: None,
            issued_at: chrono::Utc::now(),
        }];
        let live_closed_only = vec![SessionSnapshot {
            id: "c1".into(),
            client_id: Some("idle-only".into()),
            client_name: None,
            started_at_ms: 1,
            last_seen_ms: 1,
            request_count: 0,
            duration_ms: 0,
            status: SessionStatus::Closed,
        }];
        let out_idle = merge_session_clients(&registered2, &live_closed_only, &[]);
        assert_eq!(out_idle[0]["state"], "idle");

        let disk = vec![(
            "reg".into(),
            vec![
                SessionMeta {
                    client_id: "reg".into(),
                    client_name: Some("R".into()),
                    session_id: "live-active".into(),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    request_count: 9,
                    byte_size: 1,
                    status: "active".into(),
                    upstream: Some("/mcp".into()),
                },
                // Second meta for the same live session: upstream already set → skip rewrite.
                SessionMeta {
                    client_id: "reg".into(),
                    client_name: Some("R".into()),
                    session_id: "live-active".into(),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    request_count: 9,
                    byte_size: 1,
                    status: "active".into(),
                    upstream: Some("/mcp-proxy".into()),
                },
                SessionMeta {
                    client_id: "reg".into(),
                    client_name: Some("R".into()),
                    session_id: "hist".into(),
                    started_at_ms: 10,
                    ended_at_ms: Some(40),
                    request_count: 2,
                    byte_size: 1,
                    status: "closed".into(),
                    upstream: Some("/mcp-proxy".into()),
                },
            ],
        )];
        let disk_only = vec![(
            "disk".into(),
            vec![SessionMeta {
                client_id: "disk".into(),
                client_name: None,
                session_id: "d1".into(),
                started_at_ms: 5,
                ended_at_ms: None,
                request_count: 0,
                byte_size: 0,
                status: "closed".into(),
                upstream: None,
            }],
        )];
        let mut all_disk = disk;
        all_disk.extend(disk_only);
        let out = merge_session_clients(&registered, &live, &all_disk);
        assert!(out.len() >= 2);
        let reg = out.iter().find(|c| c["client_id"] == "reg").unwrap();
        let sessions = reg["sessions"].as_array().unwrap();
        let live_row = sessions.iter().find(|s| s["id"] == "live-active").unwrap();
        assert_eq!(live_row["upstream"], "/mcp");
        assert!(sessions.iter().any(|s| s["id"] == "hist"));
    }

    #[test]
    fn map_rename_error_covers_all_arms() {
        use super::map_rename_error;
        use vmcp_auth::client_store::ClientStoreError;
        use vmcp_auth::RenameClientError;

        let cases: Vec<(RenameClientError, StatusCode, &str)> = vec![
            (
                RenameClientError::InvalidName("Bad!".into()),
                StatusCode::BAD_REQUEST,
                "invalid",
            ),
            (
                RenameClientError::NameTaken("taken".into()),
                StatusCode::CONFLICT,
                "already taken",
            ),
            (
                RenameClientError::NotFound("x".into()),
                StatusCode::NOT_FOUND,
                "not found",
            ),
            (
                RenameClientError::Store(ClientStoreError::NameTaken("taken".into())),
                StatusCode::CONFLICT,
                "already taken",
            ),
            (
                RenameClientError::Store(ClientStoreError::NotFound("x".into())),
                StatusCode::NOT_FOUND,
                "not found",
            ),
            (
                RenameClientError::Store(ClientStoreError::InvalidName("Bad!".into())),
                StatusCode::BAD_REQUEST,
                "invalid",
            ),
            (
                RenameClientError::Store(ClientStoreError::Io(std::io::Error::other("disk"))),
                StatusCode::INTERNAL_SERVER_ERROR,
                "persist rename",
            ),
        ];
        for (err, want_st, needle) in cases {
            let (st, body) = map_rename_error(err);
            assert_eq!(st, want_st);
            assert!(
                body.0["error"].as_str().unwrap().contains(needle),
                "body={}",
                body.0
            );
        }
    }
}
