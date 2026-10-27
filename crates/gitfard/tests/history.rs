//! `POST /v1/log`, `/v1/show`, `/v1/blame` (git CLI executor) against a `file://` fixture upstream.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gitfard::cache::RepoCache;
use gitfard::config::ConfigHandle;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

// sha256("gfr_good") as in tests/auth.rs.
const GLOBAL: &str = "sha256:d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";
const REPO: &str = "https://example.com/acme/h.git";

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

/// Commits everything as `author` at `2026-01-<day>` in UTC+08:00, so dates and tz handling are fixed.
fn commit(work: &Path, author: &str, day: u32, msg: &str) -> String {
    git(work, &["add", "-A"]);
    let date = format!("2026-01-{day:02}T23:30:00+08:00");
    let o = Command::new("git")
        .current_dir(work)
        .args(["-c", &format!("user.name={author}"), "-c", "user.email=a@x"])
        .args(["commit", "-qm", msg])
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    git(work, &["rev-parse", "HEAD"]).trim().to_owned()
}

struct Fx {
    tmp: TempDir,
    work: PathBuf,
    config: Arc<ConfigHandle>,
    cache: Arc<RepoCache>,
    /// First and last commits on `trunk`.
    first: String,
    last: String,
}

impl Fx {
    fn new() -> Fx {
        Self::with_server("")
    }

    /// `server` is extra TOML for the `[server]` table.
    fn with_server(server: &str) -> Fx {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("d")).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        let w = |p: &str, data: &str| std::fs::write(work.join(p), data).unwrap();
        let lines = |n: usize, tag: &str| -> String {
            (1..=n).map(|i| format!("{tag} line {i}\n")).collect()
        };
        w("f.txt", &lines(600, "v0"));
        w("d/g.txt", "g\n");
        w("-dash", "dash\n");
        let first = commit(&work, "Alice Liddell", 1, "init");
        let mut last = first.clone();
        for i in 1..=29u32 {
            let mut f: Vec<String> = lines(600, "v0").lines().map(str::to_owned).collect();
            for (n, l) in f.iter_mut().enumerate() {
                if n as u32 % 29 < i {
                    *l = format!("v{} line {}", n as u32 % 29 + 1, n + 1);
                }
            }
            w("f.txt", &(f.join("\n") + "\n"));
            if i == 7 {
                w("d/g.txt", "g\ng2\n");
            }
            if i == 20 {
                // ~200 KB of patch text, to page `show` through the 64 KB cap.
                w("wide.txt", &lines(4000, &"w".repeat(40)));
            }
            let who = if i % 2 == 0 { "Alice Liddell" } else { "Bob" };
            last = commit(&work, who, 1 + i, &format!("change {i}"));
        }
        let up = tmp.path().join("up/example.com/acme/h.git");
        git(&work, &["clone", "-q", "--bare", ".", up.to_str().unwrap()]);

        let cfg = tmp.path().join("gitfard.toml");
        std::fs::write(
            &cfg,
            format!(
                "allowlist = [\"example.com/acme\"]\n\
                 [server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\n{server}\n\
                 [cache]\ndisk_budget = \"1GB\"\n\
                 [[client_tokens]]\nid = \"all\"\nhash = \"{GLOBAL}\"\n"
            ),
        )
        .unwrap();
        let config = Arc::new(ConfigHandle::load(&cfg).unwrap());
        let base = format!("file://{}/up/", tmp.path().display());
        let cache = Arc::new(
            RepoCache::open(&tmp.path().join("cache"), 2)
                .unwrap()
                .with_upstream(&base, Path::new("git")),
        );
        Fx {
            tmp,
            work,
            config,
            cache,
            first,
            last,
        }
    }

    async fn call(&self, prim: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::post(format!("/v1/{prim}"))
            .header("authorization", "Bearer gfr_good")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = gitfard::http::router(self.config.clone(), self.cache.clone())
            .oneshot(req)
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn ok(&self, prim: &str, body: Value) -> Value {
        let (status, r) = self.call(prim, body).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        r
    }

    /// Follows `next` cursors to the end, moving `trunk` after the first page; returns
    /// the stitched `text` and the page count, asserting every page pins one SHA.
    async fn all_pages(&self, prim: &str, body: Value) -> (String, usize) {
        let mut page = self.ok(prim, body).await;
        let sha = page["resolved_commit"].clone();
        let (mut text, mut pages) = (String::new(), 1);
        loop {
            let t = page["text"].as_str().unwrap();
            assert!(t.len() <= 64 * 1024);
            text += t;
            let Some(next) = page["next"].as_str() else {
                assert_eq!(page["truncated"], false);
                return (text, pages);
            };
            assert!(page["truncated_reason"].is_string(), "{page}");
            let cached = self.tmp.path().join("cache/repos/example.com/acme/h.git");
            git(&cached, &["update-ref", "refs/heads/trunk", &self.first]);
            page = self.ok(prim, json!({ "cursor": next })).await;
            assert_eq!(page["resolved_commit"], sha);
            pages += 1;
        }
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.work, args)
    }
}

fn git_log(fx: &Fx, extra: &[&str]) -> String {
    let mut args = vec![
        "log",
        "--format=%h %ad %an %s",
        "--abbrev=12",
        "--date=short",
    ];
    args.extend_from_slice(extra);
    fx.git(&args)
}

#[tokio::test]
async fn log_matches_git_log() {
    let fx = Fx::new();
    let r = fx.ok("log", json!({ "repo": REPO })).await;
    assert_eq!(r["text"], git_log(&fx, &["-n", "20", "trunk"]));
    assert_eq!(r["resolved_commit"], fx.last);
    assert_eq!(r["truncated_reason"], "limit");
}

#[tokio::test]
async fn log_filters_match_git_log() {
    let fx = Fx::new();
    let cases: [(Value, &[&str]); 5] = [
        (json!({ "path": ["d/"] }), &["trunk", "--", "d"]),
        (
            json!({ "author": "Bob", "max_count": 100 }),
            &["--author=Bob", "trunk"],
        ),
        (
            json!({ "grep": "change 1.", "max_count": 100 }),
            &["--grep=change 1.", "trunk"],
        ),
        (
            json!({ "since": "2026-01-10", "until": "2026-01-12" }),
            &["--since=2026-01-10", "--until=2026-01-12", "trunk"],
        ),
        (
            json!({ "rev": "trunk~25", "max_count": 3 }),
            &["-n", "3", "trunk~25"],
        ),
    ];
    for (mut body, args) in cases {
        body["repo"] = json!(REPO);
        let r = fx.ok("log", body.clone()).await;
        assert_eq!(r["text"], git_log(&fx, args), "{body}");
        assert!(!r["text"].as_str().unwrap().is_empty(), "{body}");
    }
}

#[tokio::test]
async fn log_cursor_pages_stitch_even_if_the_branch_moves() {
    let fx = Fx::new();
    let body = json!({ "repo": REPO, "max_count": 7 });
    let (text, pages) = fx.all_pages("log", body).await;
    assert_eq!(pages, 5);
    assert_eq!(text, git_log(&fx, &[&fx.last]));
}

#[tokio::test]
async fn show_matches_git_show_and_git_diff() {
    let fx = Fx::new();
    let cases: [(Value, &[&str]); 7] = [
        (json!({ "rev": "trunk~3" }), &["show", "trunk~3"]),
        (
            json!({ "rev": "trunk~3", "format": "stat" }),
            &["show", "--stat", "trunk~3"],
        ),
        (
            json!({ "rev": "trunk~22", "format": "name-only" }),
            &["show", "--name-only", "trunk~22"],
        ),
        (
            json!({ "rev": "trunk~3", "context_lines": 0, "path": ["f.txt"] }),
            &["show", "-U0", "trunk~3", "--", "f.txt"],
        ),
        // A range is diff only, no header.
        (
            json!({ "rev": "trunk~25..trunk~20", "path": ["d"] }),
            &["diff", "trunk~25", "trunk~20", "--", "d"],
        ),
        (
            json!({ "rev": "trunk~25..trunk~20", "format": "stat" }),
            &["diff", "--stat", "trunk~25", "trunk~20"],
        ),
        (json!({ "rev": fx.first }), &["show", &fx.first]),
    ];
    for (mut body, args) in cases {
        body["repo"] = json!(REPO);
        let r = fx.ok("show", body.clone()).await;
        assert_eq!(r["text"], fx.git(args), "{body}");
        assert_eq!(r["truncated"], false, "{body}");
    }
    let r = fx
        .ok("show", json!({ "repo": REPO, "rev": "trunk~25..trunk~20" }))
        .await;
    assert_eq!(
        r["resolved_commit"],
        fx.git(&["rev-parse", "trunk~20"]).trim()
    );
}

#[tokio::test]
async fn show_patch_pages_stitch_to_git_show() {
    let fx = Fx::new();
    // A range pins both ends, though `trunk` moves under it (to the root commit).
    let (text, pages) = fx
        .all_pages("show", json!({ "repo": REPO, "rev": "trunk~10..trunk" }))
        .await;
    assert!(pages >= 3, "{pages}");
    assert_eq!(text, fx.git(&["diff", "trunk~10", "trunk"]));

    let wide = fx.git(&["rev-parse", "trunk~9"]);
    let (text, pages) = fx
        .all_pages("show", json!({ "repo": REPO, "rev": wide.trim() }))
        .await;
    assert!(pages >= 3, "{pages}");
    assert_eq!(text, fx.git(&["show", wide.trim()]));
}

/// `git blame -l --date=short` rewritten as `<sha12> <author> <date> <line>) <text>`.
fn git_blame(fx: &Fx, range: &str, rev: &str, path: &str) -> String {
    let out = fx.git(&[
        "blame",
        "-l",
        "--root",
        "--date=short",
        "-L",
        range,
        rev,
        "--",
        path,
    ]);
    out.lines()
        .map(|l| {
            let (sha, rest) = l.split_once(" (").unwrap();
            let (meta, text) = rest.split_once(") ").unwrap();
            let mut f: Vec<&str> = meta.split_whitespace().collect();
            let line = f.pop().unwrap();
            let date = f.pop().unwrap();
            format!("{} {} {date} {line}) {text}\n", &sha[..12], f.join(" "))
        })
        .collect()
}

#[tokio::test]
async fn blame_matches_git_blame() {
    let fx = Fx::new();
    let blame = |rev: Option<&str>, path: &str, s: u32, e: u32| {
        let mut b = json!({ "repo": REPO, "path": path, "start_line": s, "end_line": e });
        if let Some(rev) = rev {
            b["rev"] = json!(rev);
        }
        b
    };
    let r = fx.ok("blame", blame(None, "f.txt", 1, 500)).await;
    assert_eq!(r["text"], git_blame(&fx, "1,500", "trunk", "f.txt"));
    assert_eq!(r["truncated"], false);
    let r = fx
        .ok("blame", blame(Some("trunk~5"), "d/g.txt", 1, 2))
        .await;
    assert_eq!(r["text"], git_blame(&fx, "1,2", "trunk~5", "d/g.txt"));
    // Clipped at EOF, like `read`.
    let r = fx.ok("blame", blame(None, "f.txt", 590, 700)).await;
    assert_eq!(r["text"], git_blame(&fx, "590,600", "trunk", "f.txt"));

    for (body, status, code) in [
        (blame(None, "f.txt", 1, 501), 400, "invalid_argument"),
        (blame(None, "f.txt", 0, 3), 400, "invalid_argument"),
        (blame(None, "f.txt", 4, 3), 400, "invalid_argument"),
        (blame(None, "d", 1, 3), 400, "is_directory"),
        (blame(None, "nope", 1, 3), 404, "path_not_found"),
    ] {
        let (s, r) = fx.call("blame", body.clone()).await;
        assert_eq!(s.as_u16(), status, "{body}");
        assert_eq!(r["error"]["code"], code, "{body}");
    }
}

#[tokio::test]
async fn user_input_starting_with_dash_is_never_a_git_option() {
    let fx = Fx::new();
    let out = fx.tmp.path().join("pwned");
    let opt = format!("--output={}", out.display());
    let o = opt.as_str();

    // As a rev: just an unknown revision.
    for (prim, body) in [
        ("log", json!({ "repo": REPO, "rev": o })),
        ("show", json!({ "repo": REPO, "rev": o })),
        (
            "show",
            json!({ "repo": REPO, "rev": format!("{o}..trunk") }),
        ),
        (
            "blame",
            json!({ "repo": REPO, "rev": o, "path": "f.txt", "start_line": 1, "end_line": 1 }),
        ),
    ] {
        let (s, r) = fx.call(prim, body.clone()).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(r["error"]["code"], "rev_not_found", "{body}");
    }
    // As a path: a literal file name.
    let r = fx
        .ok("log", json!({ "repo": REPO, "path": ["-dash"] }))
        .await;
    assert_eq!(r["text"], git_log(&fx, &["trunk", "--", "-dash"]));
    assert!(r["text"].as_str().unwrap().ends_with(" init\n"));
    let r = fx
        .ok("log", json!({ "repo": REPO, "path": [o, ":(glob)**"] }))
        .await;
    assert_eq!(r["text"], "");
    let r = fx
        .ok(
            "show",
            json!({ "repo": REPO, "rev": fx.first, "path": ["-dash"] }),
        )
        .await;
    assert!(r["text"].as_str().unwrap().contains("+++ b/-dash\n"));
    let body = json!({ "repo": REPO, "path": "-dash", "start_line": 1, "end_line": 1 });
    let r = fx.ok("blame", body).await;
    assert_eq!(r["text"], git_blame(&fx, "1,1", "trunk", "-dash"));
    // As a filter value: matched, not parsed.
    for key in ["author", "grep", "since", "until"] {
        let r = fx.ok("log", json!({ "repo": REPO, key: o })).await;
        let arg = format!("--{key}={o}");
        assert_eq!(
            r["text"],
            git_log(&fx, &[&arg, "-n", "20", "trunk"]),
            "{key}"
        );
    }
    assert!(!out.exists());
}

#[tokio::test]
async fn bad_grep_regex_is_invalid_argument() {
    let fx = Fx::new();
    let (s, r) = fx.call("log", json!({ "repo": REPO, "grep": "[" })).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["code"], "invalid_argument");
}

#[tokio::test]
async fn git_past_the_request_deadline_is_deadline_exceeded() {
    let fx = Fx::with_server("request_deadline = \"0s\"");
    for (prim, body) in [
        ("log", json!({ "repo": REPO })),
        ("show", json!({ "repo": REPO, "rev": "trunk" })),
        (
            "blame",
            json!({ "repo": REPO, "path": "f.txt", "start_line": 1, "end_line": 1 }),
        ),
    ] {
        let (s, r) = fx.call(prim, body).await;
        assert_eq!(s.as_u16(), 504, "{r}");
        assert_eq!(r["error"]["code"], "deadline_exceeded");
    }
}
