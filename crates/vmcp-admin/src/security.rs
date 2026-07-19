//! Security headers applied to every admin response.
//!
//! The admin UI is fully self-hosted now (custom `admin.css` + per-page JS
//! under `/admin/static/*`, no external CDNs and no inline `<script>`), so the
//! CSP is tightened to `script-src 'self'` only — no `unsafe-inline` scripts.
//! `style-src` still allows `'unsafe-inline'` for the handful of inline
//! `style="…"` attributes the page JS sets (e.g. `cursor:pointer`). The admin
//! is never embedded — `X-Frame-Options: DENY`. HSTS preloads on the prod
//! domain; harmless on localhost (browsers ignore it without HTTPS).
//!
//! `Cache-Control: no-cache` forces browsers to revalidate every admin
//! response (HTML pages and the `/static/*` CSS/JS) instead of serving a
//! heuristically-cached copy. `ServeDir` still emits `ETag`/`Last-Modified`,
//! so unchanged assets return a cheap `304` — but operators never get a
//! stale panel after the UI is updated.

use axum::{
    body::Body,
    http::{header, HeaderValue, Request},
    middleware::Next,
    response::Response,
};

// All page JS lives in served static assets (`/admin/static/*.js`) so
// `script-src` is locked to `'self'` — no inline scripts. `style-src` keeps
// `'unsafe-inline'` only for inline `style="…"` attributes set by that JS.
const CSP: &str = "default-src 'self'; \
                   script-src 'self'; \
                   style-src 'self' 'unsafe-inline'; \
                   font-src 'self'; \
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
    // Revalidate every admin asset so an updated panel is never masked by a
    // stale browser cache (ServeDir's ETag/Last-Modified keep this cheap via 304s).
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}
