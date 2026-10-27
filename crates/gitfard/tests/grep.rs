//! `POST /v1/grep` end to end against a `file://` fixture upstream, checked line for line
//! against `git grep -n -I -E` on the same commit.

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

// sha256("gfr_good"), as in tests/auth.rs.
const GLOBAL: &str = "sha256:d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";
const REPO: &str = "https://example.com/acme/g.git";

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    // `git grep` exits 1 when nothing matches.
    assert!(
        o.status.success() || (args[0] == "grep" && o.status.code() == Some(1)),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

struct Fx {
    _tmp: TempDir,
    work: PathBuf,
    config: Arc<ConfigHandle>,
    cache: Arc<RepoCache>,
}

impl Fx {
    fn new() -> Fx {
        Fx::with_deadline("30s")
    }

    fn with_deadline(deadline: &str) -> Fx {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("src/sub")).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        let w = |p: &str, data: &[u8]| {
            let p = work.join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, data).unwrap()
        };
        w(
            "src/main.rs",
            b"fn main() {\n    let x = 1;\n    println!(\"hello\");\n}\n",
        );
        w(
            "src/sub/a.txt",
            b"alpha\nbeta\nhello world\ngamma\ndelta\nepsilon\nzeta\nHello again\neta\n",
        );
        w("tail.txt", b"first\nno newline hello");
        w("crlf.txt", b"one hello\r\ntwo\r\n");
        w("blob.bin", b"bin\0ary hello\n");
        w("a-b", b"hello dash\n");
        w("a.b", b"hello dot\n");
        w("a/c", b"hello slash\n");
        w(
            "long.txt",
            format!("hello {}\n", "x".repeat(2000)).as_bytes(),
        );
        let many: String = (1..=250)
            .map(|i| format!("line {i}{}\n", if i % 3 == 0 { " hello" } else { "" }))
            .collect();
        w("many.txt", many.as_bytes());
        for i in 0..1500 {
            w(
                &format!("big/f{i:04}"),
                format!("f{i} hello {}\n", "_".repeat(50)).as_bytes(),
            );
        }
        // Just over 10 MB.
        w("huge.txt", &b"hello huge\n".repeat(1_000_000));
        std::os::unix::fs::symlink("hello", work.join("link")).unwrap();
        git(&work, &["add", "-A"]);
        git(
            &work,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,0123456789abcdef0123456789abcdef01234567,vendor/lib",
            ],
        );
        git(
            &work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );
        let up = tmp.path().join("up/example.com/acme/g.git");
        git(&work, &["clone", "-q", "--bare", ".", up.to_str().unwrap()]);

        let cfg = tmp.path().join("gitfard.toml");
        std::fs::write(
            &cfg,
            format!(
                "allowlist = [\"example.com/acme\"]\n\
                 [server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\nrequest_deadline = \"{deadline}\"\n\
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
            _tmp: tmp,
            work,
            config,
            cache,
        }
    }

    async fn call(&self, body: Value) -> (StatusCode, Value) {
        let req = Request::post("/v1/grep")
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

    /// Follows `next` to the end; returns the concatenated text and every page.
    async fn all_pages(&self, body: Value) -> (String, Vec<Value>) {
        let (status, mut page) = self.call(body).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let mut pages = vec![];
        let mut text = String::new();
        loop {
            text += page["text"].as_str().unwrap();
            let next = page["next"].as_str().map(str::to_owned);
            pages.push(page);
            let Some(next) = next else {
                return (text, pages);
            };
            assert!(pages.len() < 1000, "cursor does not advance");
            let (status, p) = self.call(json!({ "cursor": next })).await;
            assert_eq!(status, StatusCode::OK, "{p}");
            page = p;
        }
    }

    /// `git grep -n -I -E <args> HEAD -- <pathspec>` with the `HEAD:` prefix stripped.
    fn git_grep(&self, args: &[&str], pathspec: &[&str]) -> String {
        let mut cmd = vec!["grep", "-n", "-I", "-E"];
        cmd.extend_from_slice(args);
        cmd.push("HEAD");
        cmd.push("--");
        cmd.extend_from_slice(pathspec);
        git(&self.work, &cmd)
            // Not `lines()`: git keeps the `\r` of CRLF lines.
            .split_inclusive('\n')
            .map(|l| l.strip_prefix("HEAD:").unwrap_or(l))
            .collect()
    }
}

/// Tests leave out `big/` and `long.txt` unless about them: git caps neither pages nor line width.
/// `huge.txt` is our default `skip_large`.
const SMALL: [&str; 4] = [
    ":(glob)**",
    ":(glob,exclude)big/**",
    ":(exclude)long.txt",
    ":(exclude)huge.txt",
];

fn grep(extra: Value) -> Value {
    let mut b = json!({
        "repo": REPO,
        "pattern": "hello",
        "exclude": ["big/**", "long.txt"],
        "max_results": 10000,
    });
    b.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    b
}

#[tokio::test]
async fn lines_match_git_grep() {
    let fx = Fx::new();
    let (status, r) = fx.call(grep(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["truncated"], false);
    assert_eq!(r["text"], fx.git_grep(&["-e", "hello"], &SMALL));
    assert_eq!(r["resolved_commit"].as_str().unwrap().len(), 40);
    assert_eq!(r["skipped_large"], json!(["huge.txt"]));
}

#[tokio::test]
async fn context_modes_and_flags_match_git_grep() {
    let fx = Fx::new();
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({ "context": 1 }), vec!["-C", "1"]),
        (json!({ "before": 2 }), vec!["-B", "2"]),
        (
            json!({ "after": 3, "context": 1 }),
            vec!["-A", "3", "-B", "1"],
        ),
        (json!({ "output_mode": "files" }), vec!["-l"]),
        (json!({ "output_mode": "count" }), vec!["-c"]),
        (json!({ "ignore_case": true }), vec!["-i"]),
        (
            json!({ "pattern": "l+o (w|d)", "context": 2 }),
            vec!["-C", "2"],
        ),
        (
            json!({ "pattern": "println!(", "fixed_string": true }),
            vec!["-F"],
        ),
    ];
    for (extra, mut args) in cases {
        let pattern = extra["pattern"].as_str().unwrap_or("hello").to_owned();
        let (status, r) = fx.call(grep(extra.clone())).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        args.extend(["-e", &pattern]);
        assert_eq!(r["text"], fx.git_grep(&args, &SMALL), "{extra}");
    }
}

#[tokio::test]
async fn path_prefix_include_and_exclude_match_git_pathspecs() {
    let fx = Fx::new();
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({ "path": ["src"], "exclude": [] }), vec!["src"]),
        (
            json!({ "path": ["src/sub/", "a"], "exclude": [] }),
            vec!["src/sub", "a"],
        ),
        (
            json!({ "include": ["*.txt"] }),
            vec![":(glob)*.txt", ":(exclude)long.txt", ":(exclude)huge.txt"],
        ),
        (
            json!({ "include": ["**/*.rs", "a*"] }),
            vec![":(glob)**/*.rs", ":(glob)a*"],
        ),
        (
            json!({ "exclude": ["big/**", "long.txt", "huge.txt", "src/*"] }),
            vec![
                ":(glob)**",
                ":(glob,exclude)big/**",
                ":(exclude)long.txt",
                ":(exclude)huge.txt",
                ":(glob,exclude)src/*",
            ],
        ),
    ];
    for (extra, spec) in cases {
        let (status, r) = fx.call(grep(extra.clone())).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["text"], fx.git_grep(&["-e", "hello"], &spec), "{extra}");
    }
}

#[tokio::test]
async fn cursor_pages_concatenate_to_the_unpaged_output() {
    let fx = Fx::new();
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({ "max_results": 7 }), vec![]),
        (json!({ "max_results": 7, "context": 2 }), vec!["-C", "2"]),
        (json!({ "max_results": 1, "after": 1 }), vec!["-A", "1"]),
        (
            json!({ "max_results": 3, "output_mode": "files" }),
            vec!["-l"],
        ),
        (
            json!({ "max_results": 3, "output_mode": "count" }),
            vec!["-c"],
        ),
    ];
    for (extra, mut args) in cases {
        let (text, pages) = fx.all_pages(grep(extra.clone())).await;
        args.extend(["-e", "hello"]);
        assert_eq!(text, fx.git_grep(&args, &SMALL), "{extra}");
        assert!(pages.len() > 2, "{extra}");
        for p in &pages[..pages.len() - 1] {
            assert_eq!(p["truncated_reason"], "limit", "{extra}");
        }
    }
    // Default: 100 matches.
    let (_, r) = fx.call(json!({ "repo": REPO, "pattern": "hello" })).await;
    assert_eq!(r["text"].as_str().unwrap().lines().count(), 100);
    assert_eq!(r["truncated_reason"], "limit");
}

#[tokio::test]
async fn byte_cap_pages_and_long_lines_are_cut_to_500_chars() {
    let fx = Fx::new();
    let body =
        json!({ "repo": REPO, "pattern": "hello", "max_results": 100000, "exclude": ["huge.txt"] });
    let (_, r) = fx.call(body.clone()).await;
    assert_eq!(r["truncated_reason"], "bytes");
    assert!(r["text"].as_str().unwrap().len() <= 64 * 1024);
    let (text, _) = fx.all_pages(body).await;
    let long = text.lines().find(|l| l.starts_with("long.txt:")).unwrap();
    assert_eq!(long, format!("long.txt:1:hello {}", "x".repeat(494)));
    let not_long = |s: &str| -> String {
        s.split_inclusive('\n')
            .filter(|l| !l.starts_with("long.txt:"))
            .collect()
    };
    let git = fx.git_grep(&["-e", "hello"], &[":(exclude)huge.txt"]);
    assert_eq!(not_long(&text), not_long(&git));
}

#[tokio::test]
async fn deadline_returns_an_ordered_prefix_and_resumes() {
    let fx = Fx::with_deadline("0s");
    // Matches on both sides of a batch boundary; then only after it (the first page prints nothing).
    for pattern in ["f(2|1499) hello|line 3 ", "f1499 hello|line 3 "] {
        let body = json!({ "repo": REPO, "pattern": pattern, "context": 1,
                           "include": ["big/**", "*.txt"], "exclude": ["long.txt", "huge.txt"] });
        let (_, first) = fx.call(body.clone()).await;
        assert_eq!(first["truncated_reason"], "deadline", "{first}");
        let (text, pages) = fx.all_pages(body).await;
        assert!(pages.len() >= 2);
        let spec = [
            ":(glob)big/**",
            ":(glob)*.txt",
            ":(exclude)long.txt",
            ":(exclude)huge.txt",
        ];
        assert_eq!(text, fx.git_grep(&["-C", "1", "-e", pattern], &spec));
    }
}

#[tokio::test]
async fn skip_large_false_searches_big_blobs() {
    let fx = Fx::new();
    let body = json!({ "repo": REPO, "pattern": "hello", "include": ["huge.txt"],
                       "skip_large": false, "output_mode": "count" });
    let (_, r) = fx.call(body).await;
    assert_eq!(r["text"], "huge.txt:1000000\n");
    assert!(r.get("skipped_large").is_none());
}

#[tokio::test]
async fn bad_patterns_globs_and_limits_are_invalid_argument() {
    let fx = Fx::new();
    for extra in [
        json!({ "pattern": "(" }),
        json!({ "pattern": "a\\nb" }),
        json!({ "include": ["a["] }),
        json!({ "max_results": 0 }),
    ] {
        let (status, r) = fx.call(grep(extra.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{extra}");
        assert_eq!(r["error"]["code"], "invalid_argument");
    }
}
