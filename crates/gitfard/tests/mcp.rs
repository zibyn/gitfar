//! `/mcp` (spec §3.2) end to end with an rmcp client, against a `file://` fixture upstream.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gitfard::cache::RepoCache;
use gitfard::config::ConfigHandle;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt as _;

// sha256("gfr_good") as in tests/auth.rs.
const GLOBAL: &str = "sha256:d8ad68507997592ff320792b0431158f1063b40b9bd0d87d8f668c6b7c49281f";
const REPO: &str = "https://example.com/acme/x.git";

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}");
    String::from_utf8(o.stdout).unwrap()
}

struct Fx {
    _tmp: TempDir,
    sha: String,
    app: axum::Router,
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        std::fs::write(work.join("a.txt"), "alpha\nbeta\n").unwrap();
        let many: String = (1..=500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(work.join("many.txt"), many).unwrap();
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
                "init",
            ],
        );
        let sha = git(&work, &["rev-parse", "HEAD"]).trim().to_owned();
        let up = tmp.path().join("up/example.com/acme/x.git");
        git(&work, &["clone", "-q", "--bare", ".", up.to_str().unwrap()]);

        let cfg = tmp.path().join("gitfard.toml");
        std::fs::write(
            &cfg,
            format!(
                "allowlist = [\"example.com/acme\"]\n\
                 [server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\n\
                 public_hosts = [\"127.0.0.1\"]\nallowed_origins = [\"https://good.example:*\"]\n\
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
        let app = gitfard::http::router(config, cache);
        Fx {
            _tmp: tmp,
            sha,
            app,
        }
    }

    async fn client(&self) -> RunningService<RoleClient, ()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = self.app.clone();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let t = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
                .auth_header("gfr_good"),
        );
        ().serve(t).await.unwrap()
    }

    /// Raw `initialize` POST through the router, for HTTP-layer checks.
    async fn raw(&self, headers: &[(&str, &str)]) -> StatusCode {
        let mut req = Request::post("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"t","version":"0"}}});
        let req = req.body(Body::from(body.to_string())).unwrap();
        self.app.clone().oneshot(req).await.unwrap().status()
    }
}

async fn call(c: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().unwrap().clone();
    c.call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(args))
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn lists_six_remote_tools_that_accept_a_cursor_alone() {
    let fx = Fx::new();
    let c = fx.client().await;
    let tools = c.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["read", "grep", "list", "log", "show", "blame"]);
    for t in &tools {
        let desc = t.description.as_deref().unwrap();
        assert!(desc.contains("REMOTE repository"), "{desc}");
        assert!(t.input_schema.get("required").is_none(), "{}", t.name);
        assert!(t.input_schema["properties"].get("cursor").is_some());
        assert!(t.input_schema["properties"].get("repo").is_some());
        let out = t.output_schema.as_ref().unwrap();
        assert!(out["properties"].get("resolved_commit").is_some());
    }
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn read_and_grep_return_text_with_footer_and_structured_content() {
    let fx = Fx::new();
    let c = fx.client().await;

    let r = call(&c, "read", json!({"repo": REPO, "path": "a.txt"})).await;
    assert_eq!(r.is_error, Some(false));
    let footer = format!("[resolved_commit={}]", fx.sha);
    assert_eq!(text(&r), format!("     1\talpha\n     2\tbeta\n{footer}"));
    let s = r.structured_content.unwrap();
    assert_eq!(s["text"], "     1\talpha\n     2\tbeta\n");
    assert_eq!(s["resolved_commit"], fx.sha);

    let r = call(&c, "grep", json!({"repo": REPO, "pattern": "beta"})).await;
    assert_eq!(r.is_error, Some(false));
    assert!(text(&r).contains("a.txt"), "{}", text(&r));
    assert!(text(&r).ends_with(&footer), "{}", text(&r));
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn cursor_from_footer_continues_reading() {
    let fx = Fx::new();
    let c = fx.client().await;
    let r = call(&c, "read", json!({"repo": REPO, "path": "many.txt"})).await;
    let t = text(&r);
    let footer = t.lines().last().unwrap();
    let prefix = format!("[resolved_commit={} truncated=true next=", fx.sha);
    let next = footer
        .strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{footer}"));
    assert_eq!(r.structured_content.unwrap()["next"], next);

    let r = call(&c, "read", json!({ "cursor": next })).await;
    assert_eq!(r.is_error, Some(false), "{}", text(&r));
    assert!(text(&r).starts_with("   201\tline 201\n"), "{}", text(&r));
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn domain_errors_are_tool_errors_with_code_prefix() {
    let fx = Fx::new();
    let c = fx.client().await;
    let r = call(&c, "read", json!({"repo": REPO, "path": "nope"})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).starts_with("path_not_found: "), "{}", text(&r));
    let r = call(&c, "read", json!({"repo": REPO})).await;
    assert!(text(&r).starts_with("invalid_argument: "), "{}", text(&r));
    c.cancel().await.unwrap();
}

#[tokio::test]
async fn http_layer_rejects_missing_token_foreign_host_and_origin() {
    let fx = Fx::new();
    let auth = ("authorization", "Bearer gfr_good");
    let host = ("host", "127.0.0.1:8080");
    assert_eq!(fx.raw(&[host]).await, StatusCode::UNAUTHORIZED);
    assert_eq!(fx.raw(&[auth, host]).await, StatusCode::OK);
    assert_eq!(
        fx.raw(&[auth, ("host", "evil.example")]).await,
        StatusCode::FORBIDDEN
    );
    let origin = |o| [auth, host, ("origin", o)];
    assert_eq!(
        fx.raw(&origin("https://evil.example")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fx.raw(&origin("https://good.example")).await,
        StatusCode::OK
    );
}
