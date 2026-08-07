//! OAuth 2.1 Authorization Server + Resource Server in one process,
//! plus an Authentik facade for external IdP deployments.
//!
//! Replaces Python `vmcp/auth/{provider,jwt,store,consent}.py`. Same wire
//! contract: DCR (RFC 7591), authorization code + PKCE, JWT access tokens with
//! local JWKS rotation, Resource Indicator (RFC 8707), master-password consent.
//!
//! When `auth.provider = "authentik"`, vmcp acts only as a resource server:
//! Bearer JWTs from Authentik and/or forward-auth headers from the gateway.

#![allow(clippy::result_large_err)]

pub mod client_store;
pub mod facade;
pub mod forward_trust;
pub mod groups;
pub mod jwks;
pub mod middleware;
pub mod password;
pub mod providers;
pub mod remote_jwks;
pub mod router;
pub mod scopes;
pub mod state;
pub mod static_tokens;
pub mod tokens;
pub mod types;

pub use facade::{AuthFacade, AuthIdentity, AuthReject, AuthSource};
pub use forward_trust::ForwardAuthTrust;
pub use groups::{group_contains, scopes_from_groups, split_groups};
pub use middleware::{require_admin_scope, require_bearer};
pub use providers::authentik::{AuthentikAuth, AuthentikConfig};
pub use providers::local::LocalAuth;
pub use router::{build_external_rs_router, build_router};
pub use scopes::ScopePolicy;
pub use state::{AuthState, DcrPolicy, RenameClientError, AUTH_EPHEMERAL_MAX_AGE};
