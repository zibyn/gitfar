//! The `gitfar` binary end to end against an in-process stub server (spec §5).

use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use gitfar_proto::{ErrorBody, ErrorCode};
use serde_json::{json, Value};
use tempfile::TempDir;

const TOKEN: &str = "gfr_test";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Default)]
struct Seen {
    hits: AtomicUsize,
    /// `(primitive, body)` of every POST.
    bodies: Mutex<Vec<(String, Value)>>,
}

fn err(code: ErrorCode, msg: &str) -> Response {
    let status = StatusCode::from_u16(code.http_status()).unwrap();
    (status, Json(ErrorBody::new(code, msg))).into_response()
}

fn authed(h: &HeaderMap) -> bool {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

/// Behaviour is chosen by `repo`: an error code name fails with it, `warming` is always
/// warming, `trunc` is truncated, `nomatch` returns empty text, anything else echoes the body.
async fn prim(
    State(seen): State<Arc<Seen>>,
    Path(p): Path<String>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    seen.hits.fetch_add(1, Ordering::SeqCst);
    seen.bodies.lock().unwrap().push((p, body.clone()));
    if !authed(&h) {
        return err(
            ErrorCode::Unauthenticated,
            "missing, invalid or expired token",
        );
    }
    let ok = |text: String, extra: Value| {
        let mut v = json!({"text": text, "resolved_commit": SHA, "truncated": false});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Json(v).into_response()
    };
    if body.get("cursor").is_some() {
        return ok("page2\n".into(), json!({}));
    }
    let repo = body["repo"].as_str().unwrap_or_default();
    if let Ok(code) = serde_json::from_value::<ErrorCode>(json!(repo)) {
        return err(code, "boom");
    }
    match repo {
        "warming" => {
            let mut b = ErrorBody::new(ErrorCode::RepoWarming, "warming up");
            b.error.progress = serde_json::from_value(json!({"state":"cloning"})).unwrap();
            let mut r = (StatusCode::SERVICE_UNAVAILABLE, Json(b)).into_response();
            r.headers_mut()
                .insert(header::RETRY_AFTER, "1".parse().unwrap());
            r
        }
        "trunc" => ok(
            "a\n".into(),
            json!({"truncated": true, "truncated_reason": "limit", "next": "CUR"}),
        ),
        "nomatch" => ok(String::new(), json!({})),
        _ => ok(format!("{body}\n"), json!({})),
    }
}

async fn whoami(h: HeaderMap) -> Response {
    if !authed(&h) {
        return err(ErrorCode::Unauthenticated, "nope");
    }
    Json(json!({"token_id": "tk1", "scope": null})).into_response()
}

struct Stub {
    url: String,
    seen: Arc<Seen>,
    home: TempDir,
}

impl Stub {
    fn start() -> Stub {
        let seen = Arc::new(Seen::default());
        let app = Router::new()
            .route("/v1/whoami", get(whoami))
            .route("/v1/{p}", post(prim))
            .with_state(seen.clone());
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let l = tokio::net::TcpListener::from_std(l).unwrap();
                    axum::serve(l, app).await.unwrap();
                })
        });
        Stub {
            url,
            seen,
            home: TempDir::new().unwrap(),
        }
    }

    /// A command with a clean environment: URL and token from env, no config file.
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_gitfar"));
        c.env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("GITFAR_URL", &self.url)
            .env("GITFAR_TOKEN", TOKEN);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn last_body(&self) -> (String, Value) {
        self.seen.bodies.lock().unwrap().last().unwrap().clone()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn every_error_code_maps_to_its_exit_code_and_stderr_line() {
    let s = Stub::start();
    let expect = [
        ("invalid_argument", 2),
        ("is_directory", 2),
        ("unauthenticated", 4),
        ("not_found", 3),
        ("path_not_found", 3),
        ("rev_not_found", 3),
        ("repo_warming", 5),
        ("cache_full", 5),
        ("upstream_unavailable", 5),
        ("deadline_exceeded", 6),
        ("internal", 6),
    ];
    assert_eq!(expect.len(), ErrorCode::ALL.len());
    for (code, exit) in expect {
        let o = s.run(&["--wait", "0", "read", code, "f.txt"]);
        assert_eq!(o.status.code(), Some(exit), "{code}: {}", stderr(&o));
        assert_eq!(stderr(&o), format!("gitfar: {code}: boom\n"), "{code}");
        assert_eq!(stdout(&o), "");
    }
}

#[test]
fn cursor_with_positional_or_flag_is_a_usage_error() {
    let s = Stub::start();
    for args in [
        &["read", "r", "--cursor", "CUR"][..],
        &["read", "--cursor", "CUR", "--rev", "main"],
        &["grep", "--cursor", "CUR", "-i"],
    ] {
        let o = s.run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
    }
    assert_eq!(s.seen.hits.load(Ordering::SeqCst), 0, "nothing was sent");
}

#[test]
fn cursor_alone_continues_and_global_flags_are_allowed() {
    let s = Stub::start();
    let o = s.run(&["grep", "--cursor", "CUR", "--wait", "0"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "page2\n");
    assert_eq!(s.last_body(), ("grep".into(), json!({"cursor": "CUR"})));
}

#[test]
fn truncated_result_prints_text_and_footer_with_next() {
    let s = Stub::start();
    let o = s.run(&["list", "trunc"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), "a\n");
    assert_eq!(
        stderr(&o),
        format!("[resolved_commit={SHA} truncated=true next=CUR]\n")
    );
}

#[test]
fn json_prints_the_whole_object_and_no_footer() {
    let s = Stub::start();
    let o = s.run(&["--json", "list", "trunc"]);
    assert_eq!(o.status.code(), Some(0));
    let v: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["next"], "CUR");
    assert_eq!(stderr(&o), "");
}

#[test]
fn subcommands_send_primitive_params() {
    let s = Stub::start();
    let cases: &[(&[&str], &str, Value)] = &[
        (
            &[
                "read",
                "r",
                "a.rs",
                "--rev",
                "v1",
                "--start-line",
                "3",
                "--end-line",
                "9",
            ],
            "read",
            json!({"repo":"r","rev":"v1","path":"a.rs","start_line":3,"end_line":9}),
        ),
        (
            &[
                "grep",
                "r",
                "fn ",
                "src",
                "lib",
                "-i",
                "-F",
                "-l",
                "-A",
                "1",
                "-B",
                "2",
                "-C",
                "3",
                "-m",
                "5",
                "--include",
                "*.rs",
                "--exclude",
                "x/**",
                "--no-skip-large",
            ],
            "grep",
            json!({"repo":"r","pattern":"fn ","path":["src","lib"],"include":["*.rs"],
                   "exclude":["x/**"],"fixed_string":true,"ignore_case":true,"before":2,"after":1,
                   "context":3,"output_mode":"files","max_results":5,"skip_large":false}),
        ),
        (
            &["grep", "r", "x", "-c"],
            "grep",
            json!({"repo":"r","pattern":"x","output_mode":"count"}),
        ),
        (
            &[
                "list",
                "r",
                "src",
                "--glob",
                "*.rs",
                "--max-depth",
                "2",
                "--type",
                "file",
            ],
            "list",
            json!({"repo":"r","path":"src","glob":"*.rs","max_depth":2,"type":"file"}),
        ),
        (
            &[
                "log",
                "r",
                "a",
                "b",
                "--max-count",
                "3",
                "--since",
                "2026-01-01",
                "--until",
                "2026-02-01",
                "--author",
                "me",
                "--grep",
                "fix",
            ],
            "log",
            json!({"repo":"r","path":["a","b"],"max_count":3,"since":"2026-01-01",
                   "until":"2026-02-01","author":"me","grep":"fix"}),
        ),
        (
            &[
                "show",
                "r",
                "a..b",
                "src",
                "--format",
                "name-only",
                "--context-lines",
                "1",
            ],
            "show",
            json!({"repo":"r","rev":"a..b","path":["src"],"format":"name-only","context_lines":1}),
        ),
        (
            &["blame", "r", "a.rs", "--start-line", "1", "--end-line", "4"],
            "blame",
            json!({"repo":"r","path":"a.rs","start_line":1,"end_line":4}),
        ),
    ];
    for (args, prim, body) in cases {
        let o = s.run(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", stderr(&o));
        assert_eq!(
            &s.last_body(),
            &(prim.to_string(), body.clone()),
            "{args:?}"
        );
        assert_eq!(stderr(&o), format!("[resolved_commit={SHA}]\n"));
    }
    assert_eq!(
        s.run(&["blame", "r", "a.rs"]).status.code(),
        Some(2),
        "line range is required"
    );
}

#[test]
fn grep_without_matches_exits_1() {
    let s = Stub::start();
    assert_eq!(s.run(&["grep", "nomatch", "x"]).status.code(), Some(1));
    assert_eq!(s.run(&["read", "nomatch", "x"]).status.code(), Some(0));
}

#[test]
fn wait_0_does_not_retry_warming() {
    let s = Stub::start();
    let o = s.run(&["--wait", "0", "read", "warming", "f"]);
    assert_eq!(o.status.code(), Some(5));
    assert_eq!(stderr(&o), "gitfar: repo_warming: warming up\n");
    assert_eq!(s.seen.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn warming_is_retried_with_progress_until_wait_runs_out() {
    let s = Stub::start();
    let t = Instant::now();
    let o = s.run(&["--wait", "2", "read", "warming", "f"]);
    let took = t.elapsed();
    assert_eq!(o.status.code(), Some(5));
    let err = stderr(&o);
    assert!(err.ends_with("gitfar: repo_warming: warming up\n"), "{err}");
    assert!(
        err.lines().filter(|l| l.contains("cloning")).count() >= 2,
        "{err}"
    );
    assert!(s.seen.hits.load(Ordering::SeqCst) >= 3);
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_secs(4),
        "{took:?}"
    );
}

#[test]
fn default_wait_is_90_seconds() {
    let s = Stub::start();
    let help = stdout(&s.run(&["--help"]));
    assert!(help.contains("[default: 90]"), "{help}");
}

#[test]
fn whoami_prints_token_id_and_scope() {
    let s = Stub::start();
    let o = s.run(&["whoami"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "token_id: tk1\nscope: global\n");
}

#[test]
fn config_precedence_flag_over_env_over_file() {
    let s = Stub::start();
    let dir = s.home.path().join("gitfar");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!("url = \"{}\"\ntoken = \"{TOKEN}\"\n", s.url),
    )
    .unwrap();
    let only_file = || {
        let mut c = s.cmd();
        c.env_remove("GITFAR_URL").env_remove("GITFAR_TOKEN");
        c
    };
    assert_eq!(
        only_file().arg("whoami").output().unwrap().status.code(),
        Some(0)
    );
    let bad = "http://127.0.0.1:1";
    let o = only_file()
        .env("GITFAR_URL", bad)
        .arg("whoami")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(6), "env beats file");
    assert!(
        stderr(&o).starts_with("gitfar: network: "),
        "{}",
        stderr(&o)
    );
    let o = only_file()
        .env("GITFAR_URL", bad)
        .args(["--url", &s.url, "whoami"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "flag beats env");
    let o = only_file()
        .env("GITFAR_TOKEN", "wrong")
        .arg("whoami")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4), "env token beats file");
}

#[test]
fn missing_url_is_a_usage_error() {
    let s = Stub::start();
    let o = s
        .cmd()
        .env_remove("GITFAR_URL")
        .arg("whoami")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}
