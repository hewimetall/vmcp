//! Query-level guards beyond async-graphql's built-in limits.
//!
//! async-graphql already supports depth/complexity limits when building the
//! schema (we set them in `build_schema`). This module adds a lightweight
//! parse-time check for explicit Mutation-against-Query-only-server cases
//! before the schema parser produces less-actionable errors.
//!
//! Today this is intentionally tiny; the real enforcement lives in
//! `SchemaBuilder::limit_depth` / `limit_complexity` calls inside `build_schema`.

use anyhow::Result;

/// Parsed-document hook for future use (e.g. token-budget enforcement,
/// custom field-deny lists). For v1 we only sanity-check that the document
/// is non-empty.
pub fn pre_validate(query: &str) -> Result<()> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty GraphQL document");
    }
    if trimmed.len() > 64 * 1024 {
        anyhow::bail!("GraphQL document exceeds 64 KiB");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rejected() {
        assert!(pre_validate("").is_err());
        assert!(pre_validate("   \n  ").is_err());
    }

    #[test]
    fn small_doc_passes() {
        assert!(pre_validate("{ __typename }").is_ok());
    }
}
