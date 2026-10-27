use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gitfard::cache::RepoCache;
use gitfard::config::ConfigHandle;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// The pasted entry must make the printed token authenticate as that token's id and scope.
#[tokio::test]
async fn new_token_entry_authenticates_the_printed_token() {
    let expires: jiff::Timestamp = "2999-01-01T00:00:00Z".parse().unwrap();
    let scope = vec![
        "github.com/acme".to_string(),
        "gitlab.example.com/p".to_string(),
    ];
    let new = gitfard::token::new_token("ci-bot", &scope, Some(expires));

    let raw = new.token.strip_prefix("gfr_").expect("gfr_ prefix");
    assert_eq!(raw.len(), 43, "32 random bytes in base62");
    assert!(raw.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert!(
        new.entry.starts_with("[[client_tokens]]\n"),
        "{}",
        new.entry
    );
    assert!(!new.entry.contains(&new.token));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    let src = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"{}\"\n[cache]\ndisk_budget = \"1GB\"\ndisk_min_free = \"0B\"\n{}",
        dir.path().join("cache").display(),
        new.entry
    );
    std::fs::write(&path, src).unwrap();
    let h = Arc::new(ConfigHandle::load(&path).unwrap());
    assert_eq!(h.current().client_tokens[0].expires_at, Some(expires));

    let req = Request::get("/v1/whoami")
        .header("authorization", format!("Bearer {}", new.token))
        .body(Body::empty())
        .unwrap();
    let resp = gitfard::http::router(h.clone(), Arc::new(RepoCache::open(h).unwrap()))
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body,
        r#"{"token_id":"ci-bot","scope":["github.com/acme","gitlab.example.com/p"]}"#
    );
}

#[test]
fn tokens_are_random_and_global_entry_has_no_scope() {
    let a = gitfard::token::new_token("a", &[], None);
    let b = gitfard::token::new_token("a", &[], None);
    assert_ne!(a.token, b.token);
    assert!(!a.entry.contains("scope"), "{}", a.entry);
    assert!(!a.entry.contains("expires_at"), "{}", a.entry);
}
