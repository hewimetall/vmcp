//! Security headers applied to every admin response.
//!
//! CSP allows only `self` + jsDelivr CDN (Tabler + Grid.js). The admin is
//! never embedded — `X-Frame-Options: DENY`. HSTS preloads on the prod
//! domain; harmless on localhost (browsers ignore it without HTTPS).

use axum::{
    body::Body,
    http::{header, HeaderValue, Request},
    middleware::Next,
    response::Response,
};

// 'unsafe-inline' on script-src — every admin template embeds its
// data-fetching JS inline (servers.html does `new gridjs.Grid(...)`,
// skills.html does `fetch('/admin/api/skills')...`, etc). Without
// 'unsafe-inline' all those scripts silently fail and the tabs render
// empty. The admin is behind HTTP Basic + master password and never
// reaches a public audience, so the threat model is minimal; if we
// later move the inline JS into served static assets we can drop it.
const CSP: &str = "default-src 'self'; \
                   script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; \
                   style-src 'self' https://cdn.jsdelivr.net 'unsafe-inline'; \
                   font-src 'self' https://cdn.jsdelivr.net data:; \
                   img-src 'self' data:; \
                   connect-src 'self'; \
                   frame-ancestors 'none'";

pub async fn headers(req: Request<Body>, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    resp
}
