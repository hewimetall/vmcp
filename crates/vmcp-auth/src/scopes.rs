//! OAuth scope policy for MCP / GraphQL authorization.
//!
//! Scope string is space-separated (OAuth). Semantics:
//! - `mcp:use` — full access (legacy god-scope, backward compatible)
//! - `mcp:admin` — control-plane (`/api/v1`); also implies full MCP access
//! - `mcp:read` — Query / read_only tools only
//! - `mcp:write` — Query + Mutation
//! - `upstream:<name>` — whitelist that GraphQL namespace / proxy server
//! - `deny:<server>.<tool>` — deny-list on top of grants
//!
//! If no `upstream:*` tokens are present, all upstreams are allowed (subject to
//! read/write + deny). Empty scope → deny all (except when auth is off and no
//! claims are attached — callers treat missing policy as allow).

use std::collections::HashSet;

use crate::static_tokens::SCOPE_ADMIN;

/// Validate a space-separated scope string. Unknown tokens are rejected so a
/// typo cannot silently become deny-all at runtime (G10).
pub fn validate_scope_string(scope: &str) -> Result<(), String> {
    let mut any = false;
    for tok in scope.split_whitespace() {
        any = true;
        if matches!(tok, "mcp:use" | "mcp:admin" | "mcp:read" | "mcp:write") {
            continue;
        }
        if let Some(name) = tok.strip_prefix("upstream:") {
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                continue;
            }
            return Err(format!("invalid upstream scope token: {tok}"));
        }
        if let Some(rest) = tok.strip_prefix("deny:") {
            let mut parts = rest.splitn(2, '.');
            let server = parts.next().unwrap_or("");
            let tool = parts.next().unwrap_or("");
            if !server.is_empty()
                && !tool.is_empty()
                && server
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && tool
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                continue;
            }
            return Err(format!("invalid deny scope token: {tok}"));
        }
        return Err(format!("unknown scope token: {tok}"));
    }
    if !any {
        return Err("scope must not be empty".into());
    }
    Ok(())
}

/// Parsed authorization policy from a token/JWT `scope` claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePolicy {
    pub full: bool,
    pub read: bool,
    pub write: bool,
    /// When non-empty, only these upstream names are allowed.
    pub upstreams: HashSet<String>,
    /// Denied tools as `"server.tool"` keys.
    pub deny: HashSet<String>,
}

impl ScopePolicy {
    /// Parse a space-separated OAuth scope string.
    pub fn parse(scope: &str) -> Self {
        let mut full = false;
        let mut read = false;
        let mut write = false;
        let mut upstreams = HashSet::new();
        let mut deny = HashSet::new();

        for tok in scope.split_whitespace() {
            match tok {
                "mcp:use" | SCOPE_ADMIN => full = true,
                "mcp:read" => read = true,
                "mcp:write" => {
                    read = true;
                    write = true;
                }
                s if s.starts_with("upstream:") => {
                    let name = &s["upstream:".len()..];
                    if !name.is_empty() {
                        upstreams.insert(name.to_string());
                    }
                }
                s if s.starts_with("deny:") => {
                    let rest = &s["deny:".len()..];
                    if rest.contains('.') {
                        deny.insert(rest.to_string());
                    }
                }
                _ => {}
            }
        }

        // Full access implies read+write and no upstream whitelist restriction
        // from the full flag itself (whitelist still applies if present).
        if full {
            read = true;
            write = true;
        }

        Self {
            full,
            read,
            write,
            upstreams,
            deny,
        }
    }

    /// Authorize a tool call. `is_write` is true for Mutation / non-readOnly tools.
    pub fn authorize(&self, server: &str, tool: &str, is_write: bool) -> Result<(), String> {
        let key = format!("{server}.{tool}");
        if self.deny.contains(&key) {
            return Err(format!("scope denies {key}"));
        }
        if !self.upstreams.is_empty() && !self.upstreams.contains(server) {
            return Err(format!("scope does not grant upstream:{server}"));
        }
        if self.full {
            return Ok(());
        }
        if is_write {
            if self.write {
                return Ok(());
            }
            return Err("scope lacks mcp:write (mutation / non-readOnly tool)".into());
        }
        if self.read || self.write {
            return Ok(());
        }
        Err("scope lacks mcp:read / mcp:use".into())
    }

    /// Whether this policy may use the given upstream at all (for discovery filters).
    pub fn allows_upstream(&self, server: &str) -> bool {
        self.upstreams.is_empty() || self.upstreams.contains(server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_use_is_full_access() {
        let p = ScopePolicy::parse("mcp:use");
        assert!(p.authorize("time", "now", true).is_ok());
        assert!(p.authorize("postgres", "query", true).is_ok());
    }

    #[test]
    fn mcp_admin_is_full_access() {
        let p = ScopePolicy::parse("mcp:admin");
        assert!(p.authorize("time", "now", false).is_ok());
    }

    #[test]
    fn read_blocks_write() {
        let p = ScopePolicy::parse("mcp:read");
        assert!(p.authorize("time", "now", false).is_ok());
        assert!(p.authorize("time", "set", true).is_err());
    }

    #[test]
    fn write_allows_read_and_write() {
        let p = ScopePolicy::parse("mcp:write");
        assert!(p.authorize("time", "now", false).is_ok());
        assert!(p.authorize("time", "set", true).is_ok());
    }

    #[test]
    fn upstream_whitelist() {
        let p = ScopePolicy::parse("mcp:use upstream:time");
        assert!(p.authorize("time", "now", false).is_ok());
        assert!(p.authorize("postgres", "query", false).is_err());
        assert!(p.allows_upstream("time"));
        assert!(!p.allows_upstream("postgres"));
    }

    #[test]
    fn deny_list() {
        let p = ScopePolicy::parse("mcp:use deny:postgres.query");
        assert!(p.authorize("postgres", "list", false).is_ok());
        assert!(p.authorize("postgres", "query", false).is_err());
    }

    #[test]
    fn empty_scope_denies() {
        let p = ScopePolicy::parse("");
        assert!(p.authorize("time", "now", false).is_err());
    }

    #[test]
    fn validate_scope_string_accepts_known_and_rejects_junk() {
        assert!(validate_scope_string("mcp:use").is_ok());
        assert!(validate_scope_string("mcp:read upstream:time").is_ok());
        assert!(validate_scope_string("mcp:use deny:postgres.query").is_ok());
        assert!(validate_scope_string("").is_err());
        assert!(validate_scope_string("mcp:root").is_err());
        assert!(validate_scope_string("upstream:").is_err());
        assert!(validate_scope_string("deny:onlyserver").is_err());
    }
}
