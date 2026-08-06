//! Local OAuth AS/RS provider (vmcp-issued JWT + static `vmcp_` tokens).

use axum::http::HeaderMap;
use chrono::Utc;
use uuid::Uuid;

use crate::facade::{AuthIdentity, AuthReject, AuthSource};
use crate::state::AuthState;
use crate::static_tokens::{self, TokenInfo};
use crate::tokens::verify_access_token;

/// Local provider: static tokens + JWTs signed by this process's JWKS.
#[derive(Clone)]
pub struct LocalAuth {
    pub state: AuthState,
}

impl LocalAuth {
    pub fn new(state: AuthState) -> Self {
        Self { state }
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<AuthIdentity, AuthReject> {
        let token = match AuthFacadeBearer::extract(headers)? {
            Some(t) => t,
            None => return Err(AuthReject::MissingBearer),
        };

        if let Some(store) = &self.state.token_store {
            if token.starts_with(static_tokens::TOKEN_PREFIX) {
                return match store.lookup(token) {
                    Some(info) => Ok(synth_static(&self.state, &info)),
                    None => Err(AuthReject::InvalidToken),
                };
            }
        }

        let audiences = self.state.audience_refs();
        match verify_access_token(&self.state.jwks, token, &self.state.issuer, &audiences) {
            Ok(claims) => Ok(AuthIdentity {
                subject: claims.sub.clone(),
                client_id: claims.client_id.clone(),
                scope: claims.scope.clone(),
                groups: Vec::new(),
                issuer: claims.iss,
                audience: claims.aud,
                iat: claims.iat,
                exp: claims.exp,
                jti: claims.jti,
                source: AuthSource::LocalJwt,
            }),
            Err(_) => Err(AuthReject::InvalidToken),
        }
    }
}

/// Tiny helper so `LocalAuth` does not depend on the facade enum for Bearer parse.
struct AuthFacadeBearer;
impl AuthFacadeBearer {
    fn extract(headers: &HeaderMap) -> Result<Option<&str>, AuthReject> {
        crate::facade::AuthFacade::bearer_token(headers)
    }
}

fn synth_static(state: &AuthState, info: &TokenInfo) -> AuthIdentity {
    let now = Utc::now().timestamp();
    let exp = now + 100 * 365 * 24 * 3600;
    AuthIdentity {
        subject: info.client_id.clone(),
        client_id: info.client_id.clone(),
        scope: info.scope.clone(),
        groups: Vec::new(),
        issuer: state.issuer.clone(),
        audience: state.resource_audience.clone(),
        iat: now,
        exp,
        jti: Uuid::new_v4().to_string(),
        source: AuthSource::StaticToken,
    }
}
