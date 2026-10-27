use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gitfard::cache::RepoCache;
use gitfard::config::ConfigHandle;
use http_body_util::BodyExt;
use tower::ServiceExt;

// sha256("gfr_good"), sha256("gfr_old"), computed with sha256sum.
const GOOD: &str = "sha256:d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";
const OLD: &str = "sha256:a2626cbeb7005a9265ee3722fef10c372079f8e0d843302ff3d1fdf2439913a3";

fn write(path: &Path, tokens: &str) {
    let src = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\n[cache]\ndisk_budget = \"1GB\"\n{tokens}"
    );
    std::fs::write(path, src).unwrap();
}

fn setup(tokens: &str) -> (tempfile::TempDir, Arc<ConfigHandle>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    write(&path, tokens);
    let handle = Arc::new(ConfigHandle::load(&path).unwrap());
    (dir, handle)
}

async fn get(handle: &Arc<ConfigHandle>, uri: &str, auth: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::get(uri);
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let tmp = tempfile::tempdir().unwrap();
    let cache = Arc::new(RepoCache::open(tmp.path(), 1).unwrap());
    let resp = gitfard::http::router(handle.clone(), cache)
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn tokens() -> String {
    format!(
        "[[client_tokens]]\nid = \"ci-bot\"\nhash = \"{GOOD}\"\nscope = [\"github.com/acme\"]\n\
         [[client_tokens]]\nid = \"old\"\nhash = \"{OLD}\"\nexpires_at = \"2020-01-01T00:00:00Z\"\n"
    )
}

#[tokio::test]
async fn whoami_returns_token_id_and_scope() {
    let (_d, h) = setup(&tokens());
    let (status, body) = get(&h, "/v1/whoami", Some("Bearer gfr_good")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"token_id":"ci-bot","scope":["github.com/acme"]}"#);
}

#[tokio::test]
async fn global_token_has_null_scope() {
    let (_d, h) = setup(&format!(
        "[[client_tokens]]\nid = \"admin\"\nhash = \"{GOOD}\"\n"
    ));
    let (status, body) = get(&h, "/v1/whoami", Some("Bearer gfr_good")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"token_id":"admin","scope":null}"#);
}

#[tokio::test]
async fn missing_wrong_and_expired_tokens_get_identical_401() {
    let (_d, h) = setup(&tokens());
    let missing = get(&h, "/v1/whoami", None).await;
    let wrong = get(&h, "/v1/whoami", Some("Bearer gfr_nope")).await;
    let expired = get(&h, "/v1/whoami", Some("Bearer gfr_old")).await;
    let not_bearer = get(&h, "/v1/whoami", Some("Basic gfr_good")).await;
    assert_eq!(missing.0, StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = serde_json::from_str(&missing.1).unwrap();
    assert_eq!(body["error"]["code"], "unauthenticated");
    assert_eq!(wrong, missing);
    assert_eq!(expired, missing);
    assert_eq!(not_bearer, missing);
}

#[tokio::test]
async fn healthz_needs_no_token() {
    let (_d, h) = setup("");
    let (status, _) = get(&h, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn zero_tokens_rejects_everything() {
    let (_d, h) = setup("");
    let (status, _) = get(&h, "/v1/whoami", Some("Bearer gfr_good")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revoked_token_is_rejected_after_reload() {
    let (dir, h) = setup(&tokens());
    assert_eq!(
        get(&h, "/v1/whoami", Some("Bearer gfr_good")).await.0,
        StatusCode::OK
    );
    write(&dir.path().join("gitfard.toml"), "");
    h.reload().unwrap();
    assert_eq!(
        get(&h, "/v1/whoami", Some("Bearer gfr_good")).await.0,
        StatusCode::UNAUTHORIZED
    );
}
