//! HTML pages for the admin panel.
//!
//! Pages are intentionally thin — Grid.js fetches data from the JSON API on
//! load. This keeps the templates simple and the same data path drives both
//! humans and scripts (curl, Prometheus exporters, etc).

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Html,
    routing::get,
    Router,
};

use crate::AdminState;

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/", get(servers_index))
        .route("/servers/:name", get(server_detail))
        .route("/schema", get(schema_page))
        .route("/skills", get(skills_page))
        .route("/notifications", get(notifications_page))
        .route("/sessions", get(sessions_page))
        .route("/compare", get(compare_page))
}

#[derive(Template)]
#[template(path = "servers.html")]
struct ServersPage;

#[derive(Template)]
#[template(path = "server_detail.html")]
struct ServerDetailPage {
    name: String,
}

#[derive(Template)]
#[template(path = "schema.html")]
struct SchemaPage;

#[derive(Template)]
#[template(path = "skills.html")]
struct SkillsPage;

#[derive(Template)]
#[template(path = "notifications.html")]
struct NotificationsPage;

#[derive(Template)]
#[template(path = "sessions.html")]
struct SessionsPage;

#[derive(Template)]
#[template(path = "compare.html")]
struct ComparePage;

async fn servers_index(_state: State<AdminState>) -> Result<Html<String>, (StatusCode, String)> {
    render(ServersPage)
}

async fn server_detail(
    Path(name): Path<String>,
    _state: State<AdminState>,
) -> Result<Html<String>, (StatusCode, String)> {
    render(ServerDetailPage { name })
}

async fn schema_page(_state: State<AdminState>) -> Result<Html<String>, (StatusCode, String)> {
    render(SchemaPage)
}

async fn skills_page(_state: State<AdminState>) -> Result<Html<String>, (StatusCode, String)> {
    render(SkillsPage)
}

async fn notifications_page(
    _state: State<AdminState>,
) -> Result<Html<String>, (StatusCode, String)> {
    render(NotificationsPage)
}

async fn sessions_page(_state: State<AdminState>) -> Result<Html<String>, (StatusCode, String)> {
    render(SessionsPage)
}

async fn compare_page(_state: State<AdminState>) -> Result<Html<String>, (StatusCode, String)> {
    render(ComparePage)
}

fn render<T: Template>(tpl: T) -> Result<Html<String>, (StatusCode, String)> {
    tpl.render()
        .map(Html)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("template: {e}")))
}
