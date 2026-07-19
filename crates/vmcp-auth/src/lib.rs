//! OAuth 2.1 Authorization Server + Resource Server in one process.
//!
//! Replaces Python `vmcp/auth/{provider,jwt,store,consent}.py`. Same wire
//! contract: DCR (RFC 7591), authorization code + PKCE, JWT access tokens with
//! local JWKS rotation, Resource Indicator (RFC 8707), master-password consent.

#![allow(clippy::result_large_err)]

pub mod client_store;
pub mod jwks;
pub mod middleware;
pub mod password;
pub mod router;
pub mod state;
pub mod static_tokens;
pub mod tokens;
pub mod types;

pub use middleware::require_bearer;
pub use router::build_router;
pub use state::{AuthState, RenameClientError};
