//! `POST /v1/read` and `/v1/list` end to end, against `file://` fixture upstreams.

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

// sha256("gfr_good"), sha256("gfr_old") as in tests/auth.rs.
const GLOBAL: &str = "sha256:d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";
const SCOPED: &str = "sha256:a2626cbeb7005a9265ee3722fef10c372079f8e0d843302ff3d1fdf2439913a3";

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}

fn commit(work: &Path, msg: &str) -> String {
    git(
        work,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            msg,
        ],
    );
    git(work, &["rev-parse", "HEAD"]).trim().to_owned()
}

struct Fx {
    tmp: TempDir,
    work: PathBuf,
    config: Arc<ConfigHandle>,
    cache: Arc<RepoCache>,
    /// A commit only reachable from `side`, used to move `trunk` between cursor pages.
    side: String,
}

const REPO: &str = "https://example.com/acme/x.git";

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("src/sub")).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        let w = |p: &str, data: &[u8]| std::fs::write(work.join(p), data).unwrap();
        w("src/main.rs", b"fn main() {\n    println!(\"hi\");\n}\n");
        w("src/sub/a.txt", b"alpha\nbeta\n");
        w("tail.txt", b"no newline\nat the end");
        w("crlf.txt", b"one\r\ntwo\r\n");
        w("empty.txt", b"");
        w("blob.bin", b"bin\0ary\n");
        w("a-b", b"x\n");
        w("a.b", b"x\n");
        std::fs::create_dir_all(work.join("a")).unwrap();
        w("a/c", b"x\n");
        let many: String = (1..=2500).map(|i| format!("line {i}\n")).collect();
        w("many.txt", many.as_bytes());
        let wide: String = (1..=300)
            .map(|i| format!("{i:04}{}\n", "w".repeat(996)))
            .collect();
        w("wide.txt", wide.as_bytes());
        std::fs::create_dir_all(work.join("big")).unwrap();
        for i in 0..1500 {
            w(&format!("big/f{i:04}"), b"");
        }
        std::os::unix::fs::symlink("src/main.rs", work.join("link")).unwrap();
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
        commit(&work, "init");
        git(&work, &["checkout", "-qb", "side"]);
        w("many.txt", b"rewritten\n");
        std::fs::remove_dir_all(work.join("big")).unwrap();
        git(&work, &["add", "-A"]);
        let side = commit(&work, "side");
        git(&work, &["checkout", "-q", "trunk"]);
        let up = tmp.path().join("up/example.com/acme/x.git");
        git(&work, &["clone", "-q", "--bare", ".", up.to_str().unwrap()]);
        git(&work, &["push", "-q", up.to_str().unwrap(), "side"]);

        let cache_dir = tmp.path().join("cache").display().to_string();
        let cfg = tmp.path().join("gitfard.toml");
        std::fs::write(
            &cfg,
            format!(
                "allowlist = [\"example.com/acme\"]\n\
                 [server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"{cache_dir}\"\n[cache]\ndisk_budget = \"1GB\"\ndisk_min_free = \"0B\"\n\
                 [[client_tokens]]\nid = \"all\"\nhash = \"{GLOBAL}\"\n\
                 [[client_tokens]]\nid = \"other\"\nhash = \"{SCOPED}\"\nscope = [\"example.com/other\"]\n"
            ),
        )
        .unwrap();
        let config = Arc::new(ConfigHandle::load(&cfg).unwrap());
        let base = format!("file://{}/up/", tmp.path().display());
        let cache = Arc::new(
            RepoCache::open(config.clone())
                .unwrap()
                .with_upstream(&base, Path::new("git")),
        );
        Fx {
            tmp,
            work,
            config,
            cache,
            side,
        }
    }

    async fn call_as(&self, token: &str, prim: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::post(format!("/v1/{prim}"))
            .header("authorization", format!("Bearer {token}"))
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

    async fn call(&self, prim: &str, body: Value) -> (StatusCode, Value) {
        self.call_as("gfr_good", prim, body).await
    }

    /// Follows `next` cursors to the end, concatenating `text`; asserts every page pins one SHA.
    async fn all_pages(&self, prim: &str, body: Value, between: impl Fn()) -> (String, usize) {
        let (status, mut page) = self.call(prim, body).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let sha = page["resolved_commit"].clone();
        let (mut text, mut pages) = (String::new(), 1);
        loop {
            text += page["text"].as_str().unwrap();
            let Some(next) = page["next"].as_str() else {
                assert_eq!(page["truncated"], false);
                return (text, pages);
            };
            assert!(page["truncated_reason"].is_string(), "{page}");
            between();
            let (status, p) = self.call(prim, json!({ "cursor": next })).await;
            assert_eq!(status, StatusCode::OK, "{p}");
            assert_eq!(p["resolved_commit"], sha);
            page = p;
            pages += 1;
        }
    }

    fn cat_n(&self, path: &str) -> String {
        let o = Command::new("cat")
            .arg("-n")
            .arg(self.work.join(path))
            .output()
            .unwrap();
        String::from_utf8(o.stdout).unwrap()
    }

    fn cached(&self) -> PathBuf {
        self.tmp.path().join("cache/repos/example.com/acme/x.git")
    }

    /// `git ls-tree` rendered the way `list` prints: dirs get `/`, gitlinks `path@ <sha12>`.
    fn ls_tree(&self, args: &[&str]) -> String {
        let mut cmd = vec!["ls-tree"];
        cmd.extend_from_slice(args);
        git(&self.work, &cmd)
            .lines()
            .map(|l| {
                let (meta, path) = l.split_once('\t').unwrap();
                let f: Vec<_> = meta.split(' ').collect();
                match f[1] {
                    "tree" => format!("{path}/\n"),
                    "commit" => format!("{path}@ {}\n", &f[2][..12]),
                    _ => format!("{path}\n"),
                }
            })
            .collect()
    }
}

fn read(path: &str) -> Value {
    json!({ "repo": REPO, "path": path })
}

#[tokio::test]
async fn read_matches_cat_n() {
    let fx = Fx::new();
    for (path, lines) in [
        ("src/main.rs", 3),
        ("tail.txt", 2),
        ("crlf.txt", 2),
        ("empty.txt", 0),
    ] {
        let (status, r) = fx.call("read", read(path)).await;
        assert_eq!(status, StatusCode::OK, "{r}");
        assert_eq!(r["text"], fx.cat_n(path), "{path}");
        assert_eq!(r["total_lines"], lines, "{path}");
        assert_eq!(r["truncated"], false);
        assert_eq!(
            r["resolved_commit"],
            git(&fx.work, &["rev-parse", "trunk"]).trim()
        );
    }
}

#[tokio::test]
async fn read_line_range_and_rev() {
    let fx = Fx::new();
    let body = json!({ "repo": REPO, "path": "many.txt", "start_line": 10, "end_line": 12 });
    let (_, r) = fx.call("read", body).await;
    assert_eq!(
        r["text"],
        "    10\tline 10\n    11\tline 11\n    12\tline 12\n"
    );
    assert_eq!(r["total_lines"], 2500);
    assert_eq!(r["truncated"], false);

    let body = json!({ "repo": REPO, "rev": "side", "path": "many.txt" });
    let (_, r) = fx.call("read", body).await;
    assert_eq!(r["text"], "     1\trewritten\n");
    assert_eq!(r["resolved_commit"], fx.side);
}

#[tokio::test]
async fn read_defaults_to_200_lines_and_caps_at_2000() {
    let fx = Fx::new();
    let (_, r) = fx.call("read", read("many.txt")).await;
    assert_eq!(r["text"].as_str().unwrap().lines().count(), 200);
    assert_eq!(r["truncated_reason"], "limit");

    let body = json!({ "repo": REPO, "path": "many.txt", "start_line": 1, "end_line": 2500 });
    let (_, r) = fx.call("read", body).await;
    assert_eq!(r["text"].as_str().unwrap().lines().count(), 2000);
    assert_eq!(r["truncated_reason"], "limit");
}

#[tokio::test]
async fn read_cursor_pages_stitch_to_cat_n_even_if_the_branch_moves() {
    let fx = Fx::new();
    let repo = fx.cached();
    let side = fx.side.clone();
    let (text, pages) = fx
        .all_pages("read", read("many.txt"), || {
            git(&repo, &["update-ref", "refs/heads/trunk", &side]);
        })
        .await;
    assert_eq!(pages, 13);
    assert_eq!(text, fx.cat_n("many.txt"));
}

#[tokio::test]
async fn read_stops_at_the_64k_byte_cap_and_resumes() {
    let fx = Fx::new();
    let body = json!({ "repo": REPO, "path": "wide.txt", "start_line": 1, "end_line": 300 });
    let (_, r) = fx.call("read", body.clone()).await;
    assert_eq!(r["truncated_reason"], "bytes");
    assert!(r["text"].as_str().unwrap().len() <= 64 * 1024);
    let (text, _) = fx.all_pages("read", body, || {}).await;
    assert_eq!(text, fx.cat_n("wide.txt"));
}

#[tokio::test]
async fn read_binary_symlink_and_directory() {
    let fx = Fx::new();
    let (_, r) = fx.call("read", read("blob.bin")).await;
    assert_eq!(r["text"], "");
    assert_eq!(r["binary"], true);
    assert_eq!(r["size"], 8);
    assert!(r.get("total_lines").is_none());

    let (_, r) = fx.call("read", read("link")).await;
    assert_eq!(r["text"], "     1\tsrc/main.rs");

    for dir in ["src", "src/", ""] {
        let (status, r) = fx.call("read", read(dir)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(r["error"]["code"], "is_directory", "{dir:?}");
    }
}

#[tokio::test]
async fn domain_errors_only_after_authorization() {
    let fx = Fx::new();
    let cases = [
        (read("nope"), "path_not_found"),
        (
            json!({ "repo": REPO, "rev": "nope", "path": "tail.txt" }),
            "rev_not_found",
        ),
        (read("src"), "is_directory"),
    ];
    for (body, code) in cases {
        let (_, r) = fx.call("read", body.clone()).await;
        assert_eq!(r["error"]["code"], code);
        // Same request from a token whose scope excludes the repo, and for a repo off the allowlist.
        let (status, r) = fx.call_as("gfr_old", "read", body.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(r["error"]["code"], "not_found");
        let mut off = body;
        off["repo"] = json!("example.com/evil/x");
        let (_, r) = fx.call("read", off).await;
        assert_eq!(r["error"]["code"], "not_found");
    }
    let (_, r) = fx
        .call("list", json!({ "repo": REPO, "path": "nope" }))
        .await;
    assert_eq!(r["error"]["code"], "path_not_found");
}

#[tokio::test]
async fn bad_requests_are_invalid_argument() {
    let fx = Fx::new();
    for body in [
        json!({ "path": "x" }),
        json!({ "repo": REPO, "path": "x", "start_line": 0 }),
        json!({ "repo": REPO, "path": "x", "start_line": 5, "end_line": 4 }),
        json!({ "repo": "not a url", "path": "x" }),
        json!({ "cursor": "!!" }),
    ] {
        let (status, r) = fx.call("read", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(r["error"]["code"], "invalid_argument", "{body}");
    }
}

#[tokio::test]
async fn list_matches_ls_tree() {
    let fx = Fx::new();
    let (_, r) = fx.call("list", json!({ "repo": REPO })).await;
    assert_eq!(r["text"], fx.ls_tree(&["trunk"]));
    let (_, r) = fx
        .call("list", json!({ "repo": REPO, "path": "src" }))
        .await;
    assert_eq!(r["text"], fx.ls_tree(&["trunk", "src/"]));

    let body = json!({ "repo": REPO, "max_depth": 100 });
    let (text, _) = fx.all_pages("list", body, || {}).await;
    assert_eq!(text, fx.ls_tree(&["-r", "-t", "trunk"]));
}

#[tokio::test]
async fn list_filters_by_glob_type_and_depth() {
    let fx = Fx::new();
    let list = |body: Value| async { fx.call("list", body).await.1["text"].clone() };
    let base = json!({ "repo": REPO, "max_depth": 5 });
    let with = |extra: Value| {
        let mut b = base.clone();
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    assert_eq!(
        list(with(json!({ "glob": "**/*.rs" }))).await,
        "src/main.rs\n"
    );
    assert_eq!(
        list(with(json!({ "glob": "*.txt" }))).await,
        "crlf.txt\nempty.txt\nmany.txt\ntail.txt\nwide.txt\n"
    );
    assert_eq!(
        list(with(json!({ "type": "dir" }))).await,
        "a/\nbig/\nsrc/\nsrc/sub/\nvendor/\nvendor/lib@ 0123456789ab\n"
    );
    assert_eq!(
        list(json!({ "repo": REPO, "path": "src", "max_depth": 2, "type": "file" })).await,
        "src/main.rs\nsrc/sub/a.txt\n"
    );
    assert_eq!(
        list(json!({ "repo": REPO, "path": "tail.txt" })).await,
        "tail.txt\n"
    );
}

#[tokio::test]
async fn list_caps_at_1000_entries_and_resumes() {
    let fx = Fx::new();
    let body = json!({ "repo": REPO, "path": "big" });
    let (_, r) = fx.call("list", body.clone()).await;
    assert_eq!(r["text"].as_str().unwrap().lines().count(), 1000);
    assert_eq!(r["truncated_reason"], "limit");
    let repo = fx.cached();
    let side = fx.side.clone();
    let (text, pages) = fx
        .all_pages("list", body, || {
            git(&repo, &["update-ref", "refs/heads/trunk", &side]);
        })
        .await;
    assert_eq!(pages, 2);
    assert_eq!(text, fx.ls_tree(&["trunk", "big/"]));
}
