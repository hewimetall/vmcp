//! Static-asset regression guards for the admin SPA.
//!
//! The operator UI is a single four-zone shell (`admin.js` + `admin.css`).
//! These tests lock the class contracts and mount points so a Skills restyle
//! cannot silently break Services / Sessions / Compare / Schema / Notifications.

use std::fs;
use std::path::PathBuf;

fn static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static")
}

fn read_static(name: &str) -> String {
    let path = static_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn shared_css_contracts_intact() {
    let css = read_static("admin.css");
    let required = [
        ".shell",
        ".side1",
        ".side2",
        ".srv-item",
        ".srv-item.active",
        ".skill-detail__actions",
        ".skill-detail__title",
        ".side2__search--dark",
        ".btn--neutral",
        ".btn--block",
        ".btn--primary",
        ".btn--danger",
        ".btn--ghost",
        ".btn--sm",
        ".badge-required",
        ".code-block",
        ".pill",
        ".pill--green",
        ".nav-link",
        ".nav-link.active",
        ".modal",
        ".modal.show",
        ".modal__dialog",
        ".cmp__",
        ".sess-",
        ".xd__label--gql",
        ".code-block--gql",
        ".code-block--gql .token.keyword",
        ".page-title",
        ".table",
    ];
    for sel in required {
        assert!(
            css.contains(sel),
            "admin.css missing shared selector `{sel}`"
        );
    }
    assert!(
        !css.contains(".skill-card {") && !css.contains(".skill-tab {"),
        "admin.css must not keep accordion .skill-card / .skill-tab rules"
    );
    for needle in [
        "jsdelivr",
        "cdn.js",
        "unpkg.com",
        "gridjs",
        "@import url(",
        "https://cdn",
    ] {
        assert!(
            !css.to_lowercase().contains(needle),
            "admin.css must not pull external CSS ({needle})"
        );
    }
}

#[test]
fn spa_js_mount_points_intact() {
    let js = read_static("admin.js");
    for needle in [
        "ensureSkillsSide2",
        "renderSkillList",
        "selectSkill",
        "renderSkillDetail",
        "Search skill...",
        "/admin/api/skills/",
        "generate",
        "Last Deployed At:",
        "server-list",
        "server-search",
        "server-cards",
        "/admin/api/servers",
        "selectServer",
        "renderList",
        "ensureServersSide2",
        "openSessions",
        "openCompare",
        "openSchema",
        "openNotifications",
        "/admin/api/schema.graphql",
        "/admin/api/notifications",
        "/admin/api/sessions",
        "sessExtractGraphql",
        "sessPrettyGraphql",
        "sessGraphqlDetailHtml",
        "ensureGqlLibs",
        "sessHighlightCode",
        "/admin/static/vendor/graphql-format.min.js",
        "/admin/static/vendor/prism-core.min.js",
        "query_graphql",
        "xd__label--gql",
        "xd__label--gql\">GraphQL",
        "method: \"POST\"",
        "\"PUT\"",
        "method: \"DELETE\"",
        "method: \"PATCH\"",
        "sessSaveClientName",
        "sess-client-name",
        "saveSkill",
        "deleteSkill",
        "data-nav",
    ] {
        assert!(js.contains(needle), "admin.js missing `{needle}`");
    }
    for dead in [
        "skillCardHtml",
        "skillsHeaderHtml",
        "skill-card__toggle",
        "renderSkills()",
        "/bff/",
        "read-only — edit skills from the main",
        "DEMO_BEFORE",
        "DEMO_AFTER",
        "__demo",
        "Brave_search",
        "Mcp_atlassian",
    ] {
        assert!(
            !js.contains(dead),
            "admin.js still has dead helper `{dead}`"
        );
    }
    assert!(
        js.contains("normalizeServer"),
        "admin.js must normalize API servers without demo placeholders"
    );
}

#[test]
fn no_legacy_multipage_assets() {
    let dir = static_dir();
    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.iter().any(|n| n == "admin.css"));
    assert!(entries.iter().any(|n| n == "admin.js"));
    for dead in [
        "servers.js",
        "skills.js",
        "sessions.js",
        "compare.js",
        "schema.js",
        "notifications.js",
        "nav.js",
        "bff.js",
        "bff.css",
    ] {
        assert!(
            !entries.iter().any(|n| n == dead),
            "legacy asset `{dead}` must be removed"
        );
    }
}
