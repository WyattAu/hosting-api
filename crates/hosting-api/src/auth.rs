//! Bearer-token auth for the `/api/*` surface.
//!
//! The API is loopback-bound; this middleware exists so the edge proxy
//! can safely forward authenticated traffic without a second hop of
//! blind trust. `/healthz` and `/metrics` stay open (liveness + scraper
//! access from the tenant bridge).
//!
//! Token sources, checked in order:
//!
//! 1. **Legacy shared token** — `HOSTING_API_TOKEN` env (deployed via
//!    the Ansible role from the SOPS-managed platform secrets). The
//!    operator break-glass path; unchanged.
//! 2. **OIDC JWT** (round 7, iter 64) — when `HOSTING_OIDC_ISSUER` is
//!    set, authbox-issued `client_credentials` tokens are validated
//!    against the issuer's JWKS (estate `oauth-toolkit`, auto-
//!    discovery) and must carry the `Admin` realm role. This is the
//!    estate identity loop: authbox issues, hosting-api enforces.
//!
//! No token configured and no issuer = API closed (fail closed, not
//! open).

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;

use crate::AppState;

/// Keycloak-shaped claims accepted from OIDC issuers (the shape authbox
/// issues: subject plus realm roles).
#[derive(Debug, Clone, Deserialize)]
pub struct OidcClaims {
    /// Subject (the service client id).
    pub sub: String,
    /// Realm-role container.
    #[serde(default)]
    pub realm_access: Option<RealmRoles>,
}

/// Realm-role container.
#[derive(Debug, Clone, Deserialize)]
pub struct RealmRoles {
    /// Role names granted in the realm.
    #[serde(default)]
    pub roles: Vec<String>,
}

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

/// Extract the Bearer credentials from headers.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Check a request's bearer token against the configured static token.
/// `false` when no token is configured (fail closed).
#[allow(clippy::unused_async)] // kept async to match the middleware surface
pub async fn bearer_matches(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.api_token.as_deref() else {
        return false;
    };
    bearer(headers).is_some_and(|token| ct_eq(token.as_bytes(), expected.as_bytes()))
}

/// Validate an OIDC token against the configured issuer and require
/// the `Admin` realm role. `None` when no issuer is configured.
/// `Some(false)` on any validation/role failure (audited at warn).
pub async fn oidc_admin(state: &AppState, headers: &HeaderMap) -> Option<bool> {
    let validator = state.oidc.as_ref()?;
    let token = bearer(headers)?;
    let verdict = validator.validate_token::<OidcClaims>(token).await;
    match &verdict {
        Ok(c) => tracing::debug!(sub = %c.sub, "OIDC token validated"),
        Err(e) => tracing::debug!(error = %e, "OIDC token rejected"),
    }
    let granted = verdict.ok().is_some_and(|claims| {
        claims
            .realm_access
            .as_ref()
            .is_some_and(|r| r.roles.iter().any(|role| role == "Admin"))
    });
    Some(granted)
}

/// Axum middleware: an Admin-role JWT from the configured issuer, OR
/// the legacy static token (operator break-glass — a non-JWT string
/// can never validate as OIDC, so the fallthrough is safe).
pub async fn require_bearer(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let ok = match oidc_admin(&state, req.headers()).await {
        Some(true) => true,
        _ => bearer_matches(&state, req.headers()).await,
    };
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
