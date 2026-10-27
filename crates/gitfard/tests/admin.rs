//! Admin listener (spec §6.3), request log (§6.4) and SIGTERM (§6.6).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gitfard::cache::RepoCache;
use gitfard::config::ConfigHandle;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

// sha256("gfr_good")
const HEX: &str = "d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

/// tracing caches callsite interest globally; a request racing the log test on another thread
/// can make the log test miss its line, so fixture tests run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

struct Fx {
    _serial: std::sync::MutexGuard<'static, ()>,
    tmp: TempDir,
    config: Arc<ConfigHandle>,
    cache: Arc<RepoCache>,
}

/// One `file://` upstream `example.com/acme/x` with one commit; `extra` goes into `[cache]`.
fn fx(extra: &str) -> Fx {
    let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "trunk"]);
    std::fs::write(work.join("a.txt"), "alpha\n").unwrap();
    git(&work, &["add", "-A"]);
    git(
        &work,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "m",
        ],
    );
    let up = tmp.path().join("up/example.com/acme/x.git");
    git(&work, &["clone", "-q", "--bare", ".", up.to_str().unwrap()]);
    let cfg = tmp.path().join("gitfard.toml");
    std::fs::write(
        &cfg,
        format!(
            "allowlist = [\"example.com/acme\"]\n\
             [server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\n\
             [cache]\ndisk_budget = \"1GB\"\n{extra}\n\
             [[client_tokens]]\nid = \"ci-bot\"\nhash = \"sha256:{HEX}\"\n"
        ),
    )
    .unwrap();
    let config = Arc::new(ConfigHandle::load(&cfg).unwrap());
    let base = format!("file://{}/up/", tmp.path().display());
    let cache = Arc::new(
        RepoCache::open(&tmp.path().join("cache"), 1)
            .unwrap()
            .with_upstream(&base, Path::new("git"))
            .with_config(config.clone()),
    );
    Fx {
        _serial: serial,
        tmp,
        config,
        cache,
    }
}

impl Fx {
    async fn admin(&self, path: &str) -> (StatusCode, String) {
        let resp = gitfard::admin::router(self.config.clone(), self.cache.clone())
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn read(&self) -> (StatusCode, Value) {
        let req = Request::post("/v1/read")
            .header("authorization", "Bearer gfr_good")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"repo": "https://example.com/acme/x", "path": "a.txt"}).to_string(),
            ))
            .unwrap();
        let resp = gitfard::http::router(self.config.clone(), self.cache.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap())
    }
}

/// Checks OpenMetrics text exposition line by line; returns the declared family names.
fn parse_openmetrics(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.last(), Some(&"# EOF"), "{text}");
    let mut families = Vec::new();
    for l in &lines[..lines.len() - 1] {
        if let Some(rest) = l.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').expect(l);
            assert!(
                ["counter", "gauge", "histogram"].contains(&kind),
                "type: {l}"
            );
            families.push(name.to_owned());
        } else if l.starts_with("# HELP ") || l.starts_with("# UNIT ") {
        } else {
            let (series, value) = l.rsplit_once(' ').expect(l);
            assert!(value.parse::<f64>().is_ok(), "value: {l}");
            let name = series.split('{').next().unwrap();
            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                "name: {l}"
            );
            if let Some(labels) = series.strip_prefix(name) {
                assert!(
                    labels.is_empty() || (labels.starts_with('{') && labels.ends_with('}')),
                    "labels: {l}"
                );
            }
        }
    }
    families
}

#[tokio::test]
async fn metrics_are_openmetrics_and_cover_spec_6_3() {
    let fx = fx("disk_min_free = \"0B\"");
    let (status, body) = fx.read().await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, text) = fx.admin("/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let families = parse_openmetrics(&text);
    for name in [
        "gitfar_requests",
        "gitfar_request_duration_seconds",
        "gitfar_clone_queue_length",
        "gitfar_clones_running",
        "gitfar_clones",
        "gitfar_disk_used_bytes",
        "gitfar_disk_budget_bytes",
        "gitfar_disk_free_bytes",
        "gitfar_disk_min_free_bytes",
        "gitfar_cache_full",
        "gitfar_warm_list_over_budget",
        "gitfar_evictions",
        "gitfar_blob_cache_hits",
        "gitfar_blob_cache_misses",
        "gitfar_blob_cache_bytes",
        "gitfar_blob_cache_capacity_bytes",
        "gitfar_scan_threads",
        "gitfar_scan_threads_busy",
        "gitfar_git_processes",
        "gitfar_git_processes_limit",
    ] {
        // Labelled families appear with their first sample; nothing here truncates, fetches or
        // cools, so gitfar_truncations, gitfar_fetches, gitfar_host_cooling,
        // gitfar_host_cooldown_remaining_seconds and gitfar_warm_repo_consecutive_fetch_failures
        // are absent.
        assert!(
            families.iter().any(|f| f == name),
            "{name} missing:\n{text}"
        );
    }
    assert!(
        text.contains(r#"gitfar_requests_total{primitive="read",code="ok"}"#),
        "{text}"
    );
    assert!(
        text.contains(r#"gitfar_clones_total{result="ok"}"#),
        "{text}"
    );
    assert!(text.contains("gitfar_cache_full 0"), "{text}");
}

#[tokio::test]
async fn readyz_stays_ready_when_cache_full() {
    let fx = fx("disk_min_free = \"1000000TB\"");
    let (status, body) = fx.read().await;
    assert_eq!(body["error"]["code"], "cache_full", "{status} {body}");
    assert_eq!(fx.admin("/readyz").await.0, StatusCode::OK);
    assert_eq!(fx.admin("/healthz").await.0, StatusCode::OK);
    assert!(fx.admin("/metrics").await.1.contains("gitfar_cache_full 1"));
    let (status, text) = fx.admin("/status").await;
    assert_eq!(status, StatusCode::OK);
    let s: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(s["disk"]["cache_full"], true, "{s}");
    assert!(s["clone_queue"].as_array().unwrap().is_empty(), "{s}");
    assert!(s["cooldowns"].as_array().unwrap().is_empty(), "{s}");
}

#[tokio::test]
async fn readyz_fails_when_cache_dir_is_not_writable() {
    let fx = fx("disk_min_free = \"0B\"");
    let dir = fx.tmp.path().join("cache/tmp");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, "").unwrap();
    assert_eq!(fx.admin("/readyz").await.0, StatusCode::SERVICE_UNAVAILABLE);
}

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

#[tokio::test]
async fn one_request_line_with_token_id_and_no_secrets() {
    let logs = Captured::default();
    let w = logs.clone();
    let sub = tracing_subscriber::fmt()
        .json()
        .with_writer(move || w.clone())
        .finish();
    let fx = fx("disk_min_free = \"0B\"");
    let _g = tracing::subscriber::set_default(sub);
    let (status, body) = fx.read().await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["fields"]["message"] == "request")
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    let f = &lines[0]["fields"];
    assert_eq!(f["primitive"], "read");
    assert_eq!(f["repo"], "example.com/acme/x");
    assert_eq!(f["resolved_commit"], body["resolved_commit"]);
    assert_eq!(f["code"], "ok");
    assert_eq!(f["token"], "ci-bot");
    assert!(f["elapsed_ms"].is_u64(), "{f}");
    for secret in ["gfr_", HEX, &HEX[..8]] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[test]
fn git_version_floor_is_2_33() {
    use gitfard::admin::git_version_ok;
    assert!(git_version_ok("git version 2.33.0\n"));
    assert!(git_version_ok("git version 2.47.1.windows.1"));
    assert!(git_version_ok("git version 3.0.0"));
    assert!(!git_version_ok("git version 2.32.7"));
    assert!(!git_version_ok("git version 1.99.0"));
    assert!(!git_version_ok("garbage"));
}

/// The binary exits promptly on SIGTERM when nothing is in flight.
#[test]
fn sigterm_shuts_down() {
    let tmp = TempDir::new().unwrap();
    let cfg: PathBuf = tmp.path().join("gitfard.toml");
    std::fs::write(
        &cfg,
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\nadmin_listen = \"127.0.0.1:0\"\n\
             cache_dir = \"{}\"\nlog_format = \"json\"\n[cache]\ndisk_budget = \"1GB\"\n",
            tmp.path().join("cache").display()
        ),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gitfard"))
        .args(["serve", "--config", cfg.to_str().unwrap()])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.contains("admin listening") {
        line.clear();
        assert_ne!(out.read_line(&mut line).unwrap(), 0, "exited early");
    }
    let pid = child.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            assert!(s.success(), "{s}");
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn alert_rules_only_use_exported_metrics() {
    let fx = fx("");
    let families = parse_openmetrics(&fx.admin("/metrics").await.1);
    let rules = include_str!("../../../deploy/alerts.yml");
    let mut used = 0;
    for (i, _) in rules.match_indices("gitfar_") {
        let name: String = rules[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let family = name.strip_suffix("_total").unwrap_or(&name);
        // Labelled families appear only after their first sample, so fall back to the registry.
        let registered = format!("\"{}\", \"", &family["gitfar_".len()..]);
        assert!(
            families.iter().any(|f| f == family)
                || include_str!("../src/admin.rs").contains(&registered),
            "{name} is not exported"
        );
        used += 1;
    }
    assert!(used >= 6, "spec §6.5 lists six alerts");
}
