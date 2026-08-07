//! Caller identity forwarded on HTTP upstream `tools/call`.
//!
//! Contract (see `docs/adr/0001-caller-identity.md`):
//! - Registry `bearer` stays the **service** credential (`Authorization`).
//! - Per-call identity is sent as `X-Vmcp-*` headers (never replaces Bearer).
//! - Applied under the session `call_lock` via [`CallerSlot`] so concurrent
//!   GraphQL aliases cannot cross-contaminate.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;

/// Normalized caller for upstream authorization (subject + groups).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    pub subject: String,
    pub groups: Vec<String>,
    pub client_id: String,
    pub scope: String,
}

impl CallerIdentity {
    pub fn new(
        subject: impl Into<String>,
        groups: Vec<String>,
        client_id: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            groups,
            client_id: client_id.into(),
            scope: scope.into(),
        }
    }

    /// Comma-separated groups (adapters may also split on `|` / `;` / space).
    pub fn groups_header_value(&self) -> String {
        self.groups.join(",")
    }
}

/// Wire header names (stable contract for adapters).
pub const HEADER_SUBJECT: HeaderName = HeaderName::from_static("x-vmcp-subject");
pub const HEADER_GROUPS: HeaderName = HeaderName::from_static("x-vmcp-groups");
pub const HEADER_CLIENT_ID: HeaderName = HeaderName::from_static("x-vmcp-client-id");
pub const HEADER_SCOPE: HeaderName = HeaderName::from_static("x-vmcp-scope");

/// Hot-swappable per-call identity shared with the HTTP transport worker.
pub type CallerSlot = Arc<ArcSwap<Option<CallerIdentity>>>;

pub fn new_caller_slot() -> CallerSlot {
    Arc::new(ArcSwap::from_pointee(None))
}

/// Merge identity into rmcp `custom_headers` (skips invalid header values).
pub fn merge_identity_headers(
    mut headers: HashMap<HeaderName, HeaderValue>,
    caller: Option<&CallerIdentity>,
) -> HashMap<HeaderName, HeaderValue> {
    let Some(caller) = caller else {
        return headers;
    };
    if let Ok(v) = HeaderValue::from_str(&caller.subject) {
        headers.insert(HEADER_SUBJECT, v);
    }
    if let Ok(v) = HeaderValue::from_str(&caller.groups_header_value()) {
        headers.insert(HEADER_GROUPS, v);
    }
    if let Ok(v) = HeaderValue::from_str(&caller.client_id) {
        headers.insert(HEADER_CLIENT_ID, v);
    }
    if !caller.scope.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&caller.scope) {
            headers.insert(HEADER_SCOPE, v);
        }
    }
    headers
}

/// reqwest client that injects `X-Vmcp-*` from [`CallerSlot`] on every request.
#[derive(Clone)]
pub struct IdentityHttpClient {
    inner: reqwest::Client,
    caller: CallerSlot,
}

impl IdentityHttpClient {
    pub fn new(caller: CallerSlot) -> anyhow::Result<Self> {
        // Align with rmcp's default Streamable HTTP client (rustls via features).
        let inner = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("build reqwest client: {e}"))?;
        Ok(Self { inner, caller })
    }

    fn with_identity(
        &self,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> HashMap<HeaderName, HeaderValue> {
        let guard = self.caller.load();
        merge_identity_headers(custom_headers, guard.as_ref().as_ref())
    }
}

impl StreamableHttpClient for IdentityHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let headers = self.with_identity(custom_headers);
        self.inner
            .post_message(uri, message, session_id, auth_header, headers)
            .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let headers = self.with_identity(custom_headers);
        self.inner
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                auth_header,
                headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let headers = self.with_identity(custom_headers);
        self.inner
            .delete_session(uri, session_id, auth_header, headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let headers = self.with_identity(custom_headers);
        self.inner
            .get_stream(uri, session_id, last_event_id, auth_header, headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let headers = self.with_identity(custom_headers);
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                headers,
                max_sse_event_size,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sets_stable_header_names() {
        let id = CallerIdentity::new(
            "alice",
            vec!["mcp-users".into(), "stand-foo".into()],
            "alice",
            "mcp:use",
        );
        let h = merge_identity_headers(HashMap::new(), Some(&id));
        assert_eq!(h.get(&HEADER_SUBJECT).unwrap(), "alice");
        assert_eq!(h.get(&HEADER_GROUPS).unwrap(), "mcp-users,stand-foo");
        assert_eq!(h.get(&HEADER_CLIENT_ID).unwrap(), "alice");
        assert_eq!(h.get(&HEADER_SCOPE).unwrap(), "mcp:use");
    }
}
