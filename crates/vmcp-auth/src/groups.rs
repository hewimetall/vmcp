//! Authentik / gateway group header parsing.
//!
//! Groups arrive as a single header value with mixed separators. Matching MUST
//! be exact token equality after split — substring checks would let
//! `architect-x` inherit rights from `architect`.

/// Delimiters used in this deployment contour for `X-authentik-groups`.
const GROUP_DELIMS: &[char] = &['|', ',', ';', ' '];

/// Split a raw groups header into exact group names.
///
/// Separators: `|`, `,`, `;`, whitespace. Empty segments are dropped.
/// Order is preserved; duplicates are kept (callers may dedup if needed).
pub fn split_groups(raw: &str) -> Vec<String> {
    raw.split(GROUP_DELIMS)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Exact membership check (never substring / prefix).
pub fn group_contains(groups: &[String], required: &str) -> bool {
    groups.iter().any(|g| g == required)
}

/// Map Authentik groups onto a space-separated MCP scope string.
///
/// Only **exact** group names from `group_scopes` keys contribute scopes.
/// Missing map entries are ignored. Result is sorted for stable claims.
pub fn scopes_from_groups(
    groups: &[String],
    group_scopes: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut scopes = std::collections::BTreeSet::new();
    for g in groups {
        if let Some(mapped) = group_scopes.get(g) {
            for tok in mapped.split_whitespace() {
                if !tok.is_empty() {
                    scopes.insert(tok.to_string());
                }
            }
        }
    }
    scopes.into_iter().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn splits_all_supported_delimiters() {
        let g = split_groups("mcp-users|architect,ops;mcp-admins reader");
        assert_eq!(
            g,
            vec!["mcp-users", "architect", "ops", "mcp-admins", "reader"]
        );
    }

    #[test]
    fn exact_match_not_substring() {
        let groups = split_groups("architect-x|architect-extra");
        assert!(!group_contains(&groups, "architect"));
        assert!(group_contains(&groups, "architect-x"));
    }

    #[test]
    fn scopes_require_exact_group_key() {
        let mut map = BTreeMap::new();
        map.insert("architect".into(), "mcp:use upstream:architect_c4".into());
        map.insert("architect-x".into(), "mcp:read".into());

        let scopes = scopes_from_groups(&split_groups("architect-x"), &map);
        assert_eq!(scopes, "mcp:read");
        assert!(!scopes.contains("upstream:architect_c4"));
    }

    #[test]
    fn empty_or_whitespace_yields_no_groups() {
        assert!(split_groups("").is_empty());
        assert!(split_groups("  | , ; ").is_empty());
    }
}
