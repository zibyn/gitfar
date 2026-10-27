use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use gitfard::cache::RepoCache;
use gitfard::config::ConfigHandle;
use tower::ServiceExt;

// sha256("gfr_good")
const HEX: &str = "d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let w = self.clone();
        let sub = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || w.clone())
            .finish();
        tracing::subscriber::set_default(sub)
    }
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn config(dir: &std::path::Path, tokens: &str) -> String {
    let cache_dir = dir.join("cache").display().to_string();
    format!("[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"{cache_dir}\"\n[cache]\ndisk_budget = \"1GB\"\ndisk_min_free = \"0B\"\n{tokens}")
}

#[tokio::test]
async fn logs_never_contain_token_or_hash() {
    let logs = Captured::default();
    let _g = logs.install();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    let tokens = format!("[[client_tokens]]\nid = \"ci-bot\"\nhash = \"sha256:{HEX}\"\nexpires_at = \"2020-01-01T00:00:00Z\"\n");
    std::fs::write(&path, config(dir.path(), &tokens)).unwrap();
    let h = Arc::new(ConfigHandle::load(&path).unwrap());
    let cache = Arc::new(RepoCache::open(h.clone()).unwrap());
    for auth in ["Bearer gfr_good", "Bearer gfr_wrong"] {
        let req = Request::get("/v1/whoami").header("authorization", auth);
        gitfard::http::router(h.clone(), cache.clone())
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
    }
    let text = logs.text();
    assert!(text.contains(r#""token":"none""#), "{text}");
    for secret in ["gfr_", HEX, &HEX[..8]] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[test]
fn zero_tokens_warns_on_load_and_reload() {
    let logs = Captured::default();
    let _g = logs.install();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    std::fs::write(&path, config(dir.path(), "")).unwrap();
    let h = ConfigHandle::load(&path).unwrap();
    // Startup: main calls this once logging is initialised.
    h.current().warn_if_no_client_tokens();
    assert_eq!(
        logs.text().matches("no client_tokens").count(),
        1,
        "{}",
        logs.text()
    );
    h.reload().unwrap();
    assert_eq!(
        logs.text().matches("no client_tokens").count(),
        2,
        "{}",
        logs.text()
    );
    assert!(logs.text().contains(r#""level":"WARN""#));
}

#[test]
fn no_warning_when_tokens_exist() {
    let logs = Captured::default();
    let _g = logs.install();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    let tokens = format!("[[client_tokens]]\nid = \"ci-bot\"\nhash = \"sha256:{HEX}\"\n");
    std::fs::write(&path, config(dir.path(), &tokens)).unwrap();
    let h = ConfigHandle::load(&path).unwrap();
    h.current().warn_if_no_client_tokens();
    h.reload().unwrap();
    assert!(!logs.text().contains("no client_tokens"), "{}", logs.text());
}
