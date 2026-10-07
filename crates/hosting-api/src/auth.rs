//! Bearer-token auth for the `/api/*` surface.
//!
//! The API is loopback-bound; this middleware exists so the edge proxy
//! can safely forward authenticated traffic without a second hop of
//! blind trust. `/healthz` and `/metrics` stay open (liveness + scraper
//! access from the tenant bridge).
//!
//! Token source: `HOSTING_API_TOKEN` env (deployed via the Ansible role
//! from the SOPS-managed platform secrets). No token configured = API
//! closed (fail closed, not open).

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::AppState;

/// Constant-time byte comparison (no early exit on mismatch).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Axum middleware: verify `Authorization: Bearer <token>`.
pub async fn require_bearer(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let Some(expected) = state.api_token.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "API token not configured (HOSTING_API_TOKEN empty)",
        )
            .into_response();
    };

    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let ok = presented.is_some_and(|token| ct_eq(token.as_bytes(), expected.as_bytes()));
    if !ok {
        tracing::warn!("unauthorized API request");
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::ct_eq;

    #[test]
    fn constant_time_compare_basics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"a"));
        assert!(ct_eq(b"", b""));
    }
}
