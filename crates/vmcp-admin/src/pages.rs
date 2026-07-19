//! HTML shell for the admin SPA.
//!
//! A single Askama page (`main.html`); all data is fetched client-side from
//! the JSON API in `api.rs`. Deep links (`/skills`, `/sessions`, …) serve the
//! same shell so a hard refresh on a bookmark still boots the SPA.

use askama::Template;
use axum::{extract::State, response::Html, routing::get, Router};

use crate::AdminState;

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/", get(index))
        // Bookmark-friendly deep links — all render the same SPA shell.
        .route("/skills", get(index))
        .route("/sessions", get(index))
        .route("/compare", get(index))
        .route("/schema", get(index))
        .route("/notifications", get(index))
        .route("/servers/:name", get(index))
}

#[derive(Template)]
#[template(path = "main.html")]
struct MainPage;

async fn index(_state: State<AdminState>) -> Html<String> {
    Html(MainPage.render().expect("main.html must render"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_page_renders_shell_markers() {
        let html = MainPage.render().expect("render");
        for needle in [
            "href=\"/admin/static/admin.css\"",
            "src=\"/admin/static/admin.js\"",
            "side1__brand",
            "data-nav=\"services\"",
            "data-nav=\"skills\"",
            "data-nav=\"sessions\"",
            "data-nav=\"compare\"",
            "data-nav=\"schema\"",
            "data-nav=\"notifications\"",
            "id=\"skill-modal\"",
            "id=\"server-list\"",
            "id=\"server-cards\"",
        ] {
            assert!(html.contains(needle), "main.html missing `{needle}`");
        }
        // No CDN / Grid.js leftovers from the old multi-page admin.
        for dead in ["jsdelivr", "gridjs", "tabler", "/bff/"] {
            assert!(
                !html.to_lowercase().contains(dead),
                "main.html must not reference `{dead}`"
            );
        }
    }
}
