//! HTTP API (spec §3.1): router and the bearer middleware for 客户端访问令牌 (§4.2).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, MethodRouter};
use axum::{Extension, Json, Router};
use gitfar_proto::{ErrorBody, ErrorCode, Primitive};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::cache::RepoCache;
use crate::config::ConfigHandle;

/// The authenticated caller, inserted into request extensions by the bearer middleware.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenCtx {
    pub token_id: String,
    /// `None` = global.
    pub scope: Option<Vec<String>>,
}

pub fn router(config: Arc<ConfigHandle>, cache: Arc<RepoCache>) -> Router {
    let prim = |p: Primitive| primitive(p, config.clone(), cache.clone());
    Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/read", prim(Primitive::Read))
        .route("/v1/list", prim(Primitive::List))
        .route("/v1/grep", prim(Primitive::Grep))
        .layer(middleware::from_fn_with_state(config, bearer))
        .route("/healthz", get(|| async { "ok" }))
}

/// `POST /v1/{primitive}`: any body problem, including invalid JSON, is `invalid_argument`.
fn primitive(p: Primitive, config: Arc<ConfigHandle>, cache: Arc<RepoCache>) -> MethodRouter {
    post(
        move |Extension(t): Extension<TokenCtx>, body: Bytes| async move {
            let res = match serde_json::from_slice(&body) {
                Ok(body) => crate::exec::run(&config, &cache, t.scope.as_deref(), p, body).await,
                Err(e) => Err(ErrorBody::new(ErrorCode::InvalidArgument, e.to_string())),
            };
            match res {
                Ok(v) => Json(v).into_response(),
                Err(e) => error(e),
            }
        },
    )
}

fn error(body: ErrorBody) -> Response {
    let code = body.error.code;
    let status = StatusCode::from_u16(code.http_status()).expect("valid status");
    let mut resp = (status, Json(body)).into_response();
    if code == ErrorCode::RepoWarming {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
    }
    resp
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
