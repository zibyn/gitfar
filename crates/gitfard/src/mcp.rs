//! MCP endpoint (spec §3.2): the six 检索原语 as tools over stateless Streamable HTTP.

use std::sync::Arc;

use axum::http::request::Parts;
use gitfar_proto::{ErrorBody, Primitive};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{json, Value};

use crate::cache::RepoCache;
use crate::config::ConfigHandle;
use crate::http::TokenCtx;

#[derive(Clone)]
struct Mcp {
    config: Arc<ConfigHandle>,
    cache: Arc<RepoCache>,
}

/// The `/mcp` service. Mount it behind the bearer middleware.
pub fn service(
    config: Arc<ConfigHandle>,
    cache: Arc<RepoCache>,
) -> StreamableHttpService<impl ServerHandler, NeverSessionManager> {
    let server = config.current().server.clone();
    let mut http = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_origins(server.allowed_origins)
        .enforce_origin_validation();
    // shortcut: hosts/origins are read once; a SIGHUP reload doesn't change them.
    // Empty `public_hosts` keeps rmcp's loopback-only default rather than allowing every Host.
    if !server.public_hosts.is_empty() {
        http = http.with_allowed_hosts(server.public_hosts);
    }
    let mcp = Mcp { config, cache };
    StreamableHttpService::new(
        move || Ok(mcp.clone()),
        Arc::new(NeverSessionManager::default()),
        http,
    )
}

fn description(p: Primitive) -> String {
    let what = match p {
        Primitive::Read => "Read a file with line numbers; path is required.",
        Primitive::Grep => "Regex search across files; pattern is required.",
        Primitive::List => "List files under a directory.",
        Primitive::Log => "Commit history, optionally limited to paths.",
        Primitive::Show => "A commit's message and diff; rev is required.",
        Primitive::Blame => "Blame a line range; path, start_line and end_line are required.",
    };
    format!(
        "Searches a REMOTE repository (repo parameter required), not the local workspace. {what} \
         To continue a truncated result, call again with only the `next` cursor from its footer."
    )
}

/// The params schema, loosened so a cursor-only call `{cursor}` validates at strict clients.
fn input_schema(p: Primitive) -> Arc<JsonObject> {
    let mut o = p.params_schema().as_object().cloned().unwrap_or_default();
    o.remove("required");
    if let Some(props) = o.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert(
            "cursor".into(),
            json!({"type": "string", "description": "`next` cursor from a truncated result; send it alone."}),
        );
    }
    Arc::new(o)
}

fn tool_result(res: Result<Value, ErrorBody>) -> CallToolResult {
    let r = match res {
        Ok(r) => r,
        Err(e) => {
            let code = serde_json::to_value(e.error.code).unwrap_or_default();
            let code = code.as_str().unwrap_or("internal");
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "{code}: {}",
                e.error.message
            ))]);
        }
    };
    let sha = r["resolved_commit"].as_str().unwrap_or_default();
    let footer = match r["next"].as_str() {
        Some(next) => format!("[resolved_commit={sha} truncated=true next={next}]"),
        None => format!("[resolved_commit={sha}]"),
    };
    let text = format!("{}{footer}", r["text"].as_str().unwrap_or_default());
    let mut out = CallToolResult::success(vec![ContentBlock::text(text)]);
    out.structured_content = Some(r);
    out
}

impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("gitfard", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = Primitive::ALL.map(|p| {
            let out = p.response_schema().as_object().cloned().unwrap_or_default();
            Tool::new(p.name(), description(p), input_schema(p))
                .with_raw_output_schema(Arc::new(out))
        });
        Ok(ListToolsResult::with_all_items(tools.to_vec()))
    }

    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(p) = Primitive::ALL.into_iter().find(|p| p.name() == req.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {}", req.name),
                None,
            ));
        };
        // The bearer middleware ran first; no TokenCtx means the service was mounted wrong.
        let token = ctx
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<TokenCtx>())
            .ok_or_else(|| ErrorData::internal_error("no authenticated caller", None))?;
        let args = Value::Object(req.arguments.unwrap_or_default());
        let res =
            crate::exec::run(&self.config, &self.cache, token.scope.as_deref(), p, args).await;
        Ok(tool_result(res).into())
    }
}
