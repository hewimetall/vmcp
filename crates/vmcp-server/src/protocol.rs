//! MCP protocol-version advertisement for `/mcp` and `/mcp-proxy`.
//!
//! `[mcp].latest` (default off) is the gateway-level feature flag: when false,
//! `server/discover` / `initialize` only list the legacy era (`2025-11-25` and
//! earlier). When true, `2026-07-28` is added to `supportedVersions` and rmcp
//! serves those requests on the stateless path.

use std::borrow::Cow;

use rmcp::model::ProtocolVersion;

/// Versions advertised when `[mcp].latest` is off.
///
/// Derived from [`ProtocolVersion::KNOWN_VERSIONS`] so a future rmcp bump that
/// adds another pre-2026 revision is picked up automatically.
pub fn advertised_protocol_versions(latest: bool) -> Cow<'static, [ProtocolVersion]> {
    if latest {
        Cow::Borrowed(ProtocolVersion::KNOWN_VERSIONS)
    } else {
        Cow::Owned(
            ProtocolVersion::KNOWN_VERSIONS
                .iter()
                .filter(|v| v.as_str() < ProtocolVersion::V_2026_07_28.as_str())
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_off_excludes_2026() {
        let versions = advertised_protocol_versions(false);
        assert!(
            versions
                .iter()
                .any(|v| v.as_str() == ProtocolVersion::V_2025_11_25.as_str()),
            "legacy list must include 2025-11-25, got {versions:?}"
        );
        assert!(
            versions
                .iter()
                .all(|v| v.as_str() < ProtocolVersion::V_2026_07_28.as_str()),
            "latest=false must not advertise 2026-07-28, got {versions:?}"
        );
    }

    #[test]
    fn latest_on_includes_2026() {
        let versions = advertised_protocol_versions(true);
        assert!(
            versions
                .iter()
                .any(|v| v.as_str() == ProtocolVersion::V_2026_07_28.as_str()),
            "latest=true must advertise 2026-07-28, got {versions:?}"
        );
        assert!(
            versions
                .iter()
                .any(|v| v.as_str() == ProtocolVersion::V_2025_11_25.as_str()),
            "latest=true stays dual-era and still lists 2025-11-25, got {versions:?}"
        );
    }
}
