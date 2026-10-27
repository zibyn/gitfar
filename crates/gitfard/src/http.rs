//! HTTP API (spec §3.1): router and the bearer middleware for 客户端访问令牌 (§4.2).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use gitfar_proto::{ErrorBody, ErrorCode};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::ConfigHandle;

/// The authenticated caller, inserted into request extensions by the bearer middleware.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenCtx {
    pub token_id: String,
    /// `None` = global.
    pub scope: Option<Vec<String>>,
}

pub fn router(config: Arc<ConfigHandle>) -> Router {
    Router::new()
        .route("/v1/whoami", get(whoami))
        .layer(middleware::from_fn_with_state(config, bearer))
        .route("/healthz", get(|| async { "ok" }))
}

async fn whoami(Extension(t): Extension<TokenCtx>) -> Json<TokenCtx> {
    Json(t)
}

async fn bearer(State(config): State<Arc<ConfigHandle>>, mut req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.get(..7)
                .filter(|s| s.eq_ignore_ascii_case("bearer "))
                .map(|_| &v[7..])
        });
    let Some(presented) = presented else {
        return unauthenticated("missing bearer token");
    };
    let digest = hex::encode(Sha256::digest(presented.as_bytes()));
    let cfg = config.current();
    // Compare against every entry without early exit, so timing does not reveal which one matched.
    let mut hit = None;
    for t in &cfg.client_tokens {
        let stored = t.hash.expose().trim_start_matches("sha256:");
        if bool::from(stored.as_bytes().ct_eq(digest.as_bytes())) {
            hit = Some(t);
        }
    }
    let Some(t) = hit else {
        return unauthenticated("unknown token");
    };
    if t.expires_at.is_some_and(|e| e <= jiff::Timestamp::now()) {
        // Still `token=none`: an unauthenticated request has no identity in logs.
        return unauthenticated("expired token");
    }
    req.extensions_mut().insert(TokenCtx {
        token_id: t.id.clone(),
        scope: t.scope.clone(),
    });
    next.run(req).await
}

/// Same body for every cause (spec §4.2); the cause only goes to the log.
fn unauthenticated(reason: &str) -> Response {
    tracing::info!(token = "none", reason, "authentication failed");
    let body = ErrorBody::new(
        ErrorCode::Unauthenticated,
        "missing, invalid or expired token",
    );
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}
