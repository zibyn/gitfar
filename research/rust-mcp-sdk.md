# Rust MCP SDK：Streamable HTTP 与认证支持现状

工单：zibyn/gitfar#5（map #1）。调研日期：2026-10-27。
源码快照：`modelcontextprotocol/rust-sdk` @ `cd88056`（2026-10-26），MCP spec repo `docs/specification/2026-07-28`。

## 结论（TL;DR）

- **选 `rmcp`（官方 SDK）**，`features = ["server", "macros", "transport-streamable-http-server"]`。它是 tower `Service`，直接 `axum::Router::nest_service("/mcp", svc)` 挂到 gitfar 的 HTTP API 同一进程/同一路由树上。
- **认证用我们自己的 axum/tower 中间件校验 bearer token**，不靠 SDK。rmcp 的 `auth` feature 是*客户端* OAuth；服务端没有内置 token 校验，官方示例就是 `middleware::from_fn` 校验 `Authorization: Bearer`。tool handler 可通过 `Extension<http::request::Parts>` 读取请求头/中间件放入的身份。
- **按无状态部署**：`legacy_session_mode = false`（或 `NeverSessionManager`），`json_response = true`。新协议 2026-07-28 本身已删除 session，rmcp 对新版本请求总是无状态处理；只读检索工具天然无状态，水平扩展无需粘性会话。
- **不必在 v1 实现完整 OAuth 2.1**：spec 规定授权对 MCP 实现是 OPTIONAL；Claude Code、Codex、Cursor 都支持静态 bearer header。若以后要 OAuth，服务端只需做资源服务器部分（RFC 9728 元数据 + `WWW-Authenticate` + audience 校验），授权服务器可外接。
- 必须改的默认值：`allowed_hosts` 默认只允许 loopback，公网部署必须设成自己的域名，否则请求被拒。

## 1. 官方 SDK `rmcp`

### 成熟度与发布节奏

| 项 | 值 | 来源 |
|---|---|---|
| 最新版本 | 3.8.0（2026-10-26） | crates.io API `/api/v1/crates/rmcp`；`gh release list -R modelcontextprotocol/rust-sdk` |
| 大版本 | 1.0.0 2026-03-03，2.0.0 2026-06-29，3.0.0 2026-07-28（与 spec 2026-07-28 同日） | crates.io API |
| 节奏 | 2026-08 至 10 月约每 1–2 周一个 minor/patch（3.1.x → 3.8.0） | `crates/rmcp/CHANGELOG.md` |
| 下载量 | 总计约 4067 万，近 90 天约 2282 万 | crates.io API |
| GitHub | ~4.0k stars，持续活跃 | `gh api repos/modelcontextprotocol/rust-sdk` |
| SDK 分级 | 自称满足 SEP-1730 **Tier 1**；2025-11-25 与 2026-07-28 两套 conformance 服务端 30/30、客户端 100% | `ROADMAP.md` |

风险：一年内 3 个大版本，大版本跟随 spec 变动（2.x→3.x 对应无状态协议）。需锁定 major 版本，并预期 spec 每次大改时有迁移成本。版本策略见 `VERSIONING.md`。

### 支持的 spec 版本

`crates/rmcp/src/model.rs`：`ProtocolVersion` 定义了 `2024-11-05`、`2025-03-26`、`2025-06-18`、`2025-11-25`、`2026-07-28`，`LATEST = V_2026_07_28`，`LATEST_WITH_INITIALIZE = V_2025_11_25`。也就是说，新的无状态协议和带 `initialize` 握手的旧协议，同一个服务都能处理。`server/discover`（2026-07-28 要求服务端实现）在 `handler/server.rs` 中已处理。

### 服务端 Streamable HTTP 传输

- feature：`transport-streamable-http-server`（会话管理器另见 `transport-streamable-http-server-session`）。来源：crates.io 上 3.8.0 的 feature 列表。
- 类型：`StreamableHttpService<S, M>` 实现了 `tower_service::Service<http::Request<B>>`（`streamable_http_server/tower.rs:1354`），所以能挂到任何 tower/hyper 栈上；examples 用的是 axum 0.8（`examples/servers/src/counter_streamhttp.rs`：`Router::new().nest_service("/mcp", service)`），也有纯 hyper 示例 `counter_hyper_streamable_http.rs`。rmcp 本身不依赖 axum（axum 只在 dev-dependencies，`crates/rmcp/Cargo.toml:211`）。
- 构造：`StreamableHttpService::new(|| Ok(Handler::new()), session_manager, StreamableHttpServerConfig)`，factory 对每个会话/请求生成 handler。
- `StreamableHttpServerConfig`（`tower.rs:78-211`）的关键字段与默认值：
  - `legacy_session_mode: true`：只影响 `< 2026-07-28` 的旧版本请求；按 SEP-2567，2026-07-28 的请求“always served statelessly regardless of this setting”。
  - `json_response: false`：无状态时可改用 `application/json` 返回；handler 在最终结果前发出通知时会自动回退到 SSE。
  - `allowed_hosts: ["localhost","127.0.0.1","::1"]`：做 Host 校验防 DNS rebinding，**公网部署必须改**。
  - `allowed_origins: []`：默认不校验 Origin。spec 要求服务端 MUST 校验 Origin（`streamable-http.mdx:58`），所以应配置；对非浏览器 agent，请求不带 Origin 时照样放行。
  - `session_store`：可选的外部会话存储，旧版本有状态模式下用于多实例恢复。
  - `max_request_body_bytes`、`sse_keep_alive`（15s）、`cancellation_token`（用于优雅停机）。
- 会话管理器：`session::local::LocalSessionManager`（进程内）、`session::never::NeverSessionManager`（完全无状态）、`session::store`（外部存储）。来源：`streamable_http_server/session/` 和 `tower.rs` 文档注释。

### 认证

- **服务端：SDK 不内置 token 校验**，做法是在外层套 tower/axum 中间件。官方示例 `examples/servers/src/simple_auth_streamhttp.rs`：`Router::new().nest_service("/mcp", mcp_service).layer(middleware::from_fn_with_state(token_store, auth_middleware))`，中间件解析 `Authorization: Bearer`，失败返回 401。
- 在 tool 中读取身份：rmcp 会把 `http::request::Parts` 注入请求的 `Extensions`（`tower.rs:1270-1330`）。tool 参数可以写 `Extension(parts): Extension<http::request::Parts>`，也可以用 `ctx.extensions.get::<Parts>()`。中间件写进 `parts.extensions` 的 principal（比如 token 对应的 org/权限）能在这里取到，正好用来做“缓存读取须校验同等权限”。
- **OAuth 2.1 资源服务器**：没有现成的服务端组件。`examples/servers/src/complex_auth_streamhttp.rs` 用 axum 手写了 `/.well-known/oauth-protected-resource/mcp`、`/oauth/authorize`、`/oauth/token` 等端点作演示，并非库 API。
- **客户端 OAuth**：`transport/auth.rs`（feature `auth`、`auth-client-credentials-jwt`、`auth-enterprise-managed`）提供 `AuthorizationManager`、metadata 发现、CIMD、DCR、PKCE、client credentials、token refresh，这些对 gitfar 的 Rust CLI 可能有用。

### Tool schema 生成

`#[tool_router]`、`#[tool(description = ...)]`、`#[tool_handler]` 宏（feature `macros`，crate `rmcp-macros`）配合 `schemars` 1.x，由参数结构体的 `JsonSchema` derive 自动生成 `inputSchema`。示例见 `examples/servers/src/common/counter.rs`，结构化输出见 `examples/servers/src/structured_output.rs`。`server` feature 默认带上 `schemars`（`Cargo.toml:133`）。

## 2. MCP spec 2026-07-28 要点（影响服务端设计）

来源：`modelcontextprotocol/modelcontextprotocol` 仓库 `docs/specification/2026-07-28/changelog.mdx`、`basic/transports/streamable-http.mdx`、`basic/authorization/index.mdx`。

- 删除协议级 session 和 `Mcp-Session-Id`（SEP-2567），删除 `initialize` 握手，每个请求在 `_meta` 里带版本和能力（SEP-2575）；新增 `server/discover`（MUST）。
- 删除 GET 流端点，改为 `subscriptions/listen` 长连接 POST；删除 SSE 断点续传和 `Last-Event-ID`。
- 新增请求头 `MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name`，便于网关按方法路由或限流。
- 服务端 MUST 校验 `Origin`，非法时返回 403。
- 授权：“Authorization is **OPTIONAL** for MCP implementations”。如果实现，HTTP 传输 SHOULD 遵循 spec：MCP 服务器 MUST 实现 RFC 9728 Protected Resource Metadata，MUST 校验 token audience 是自己，无效 token MUST 返回 401，token MUST NOT 出现在 query string 中，且每个 HTTP 请求都要带 Authorization。

这些变化对 gitfar 有利：只读检索本来就是无状态的请求/响应，无需会话亲和，多副本直接放在负载均衡后面即可。

## 3. 其他 Rust SDK

| crate | 版本/日期 | 下载（总/近期） | 说明 | 来源 |
|---|---|---|---|---|
| `rust-mcp-sdk`（rust-mcp-stack） | 2.0.0 / 2026-08-27 | 32万 / 11.6万 | 2.x 实现 2026-07-28，1.x LTS 对应 2025-11-25；README 称通过 100% conformance；提供 axum（`rust-mcp-axum`）和 actix 后端；**内置服务端 OAuth**（远程 OAuth provider，WorkOS/Scalekit 通过 `rust-mcp-extra` 接入）；~200 stars | README（`gh api repos/rust-mcp-stack/rust-mcp-sdk/readme`）、crates.io |
| `pmcp`（paiml） | 2.22.7 / 2026-10-04 | 14万 / 6.4万 | 社区 SDK，版本迭代频繁 | crates.io（未深入核查） |
| `turbomcp`（Epistates） | 4.0.0-alpha.5 / 2026-10-01 | 3.8万 / 1.2万 | 当前大版本仍是 alpha | crates.io（未深入核查） |

`rust-mcp-sdk` 唯一的明显优势是自带服务端 OAuth 资源服务器支持。gitfar v1 用自管 access token，用不上这一点，而 rmcp 的使用量高两个数量级，又是官方 Tier 1，所以选 rmcp。

## 4. MCP 客户端对远程 Streamable HTTP + bearer 的支持

| 客户端 | 配置方式 | OAuth | 来源 |
|---|---|---|---|
| Claude Code | `claude mcp add --transport http <name> <url> --header "Authorization: Bearer <token>"`；JSON 写法 `{"type":"http","url":...,"headers":{...}}`，`streamable-http` 是 `http` 的别名；headers 支持 `${VAR}`；`headersHelper` 可以动态取 token | 支持（`/mcp` 或 `claude mcp login`）。配置了静态 header 时不会回退到 OAuth | https://code.claude.com/docs/en/mcp |
| Codex | `~/.codex/config.toml` 中 `[mcp_servers.<name>]`：`url`、`bearer_token_env_var`、`http_headers`、`env_http_headers` | 支持（CIMD、DCR，`codex mcp login`）；会先尝试显式配置的 bearer | https://learn.chatgpt.com/docs/extend/mcp?surface=cli（原 developers.openai.com/codex/mcp） |
| Cursor | `.cursor/mcp.json` / `~/.cursor/mcp.json`：`mcpServers.<name>.url` + `headers`（可用 `${env:VAR}`） | 支持浏览器 OAuth | **官方文档本次抓取失败**，依据的是二手资料（如 designrevision.com、fast.io）；Cursor 论坛有 CLI 不发送 headers 的 bug 报告：https://forum.cursor.com/t/cursor-cli-cant-use-remote-mcp-servers-or-local-with-auth-requirements/134112。需要人工到 https://cursor.com/docs 核实 |

三者都支持“URL + 静态 bearer header”，所以 gitfar 以 access token 作为 v1 鉴权方式是可行的。

## 5. 对 gitfar 的建议形态

```rust
let mcp = StreamableHttpService::new(
    || Ok(GitfarTools::new(state.clone())),
    Arc::new(NeverSessionManager::default()),
    StreamableHttpServerConfig {
        legacy_session_mode: false,
        json_response: true,
        allowed_hosts: vec![cfg.public_host.clone()],
        ..Default::default()
    },
);
let app = Router::new()
    .nest("/api", api_routes)
    .nest_service("/mcp", mcp)
    .layer(from_fn_with_state(tokens, bearer_auth)); // HTTP API 与 MCP 共用同一鉴权中间件
```

（示意代码，未编译；字段名以 rmcp 3.8 的 `tower.rs` 为准。）

- 401 响应可以先带上 `WWW-Authenticate: Bearer`，为以后接 OAuth 留出口子。到时再补 `/.well-known/oauth-protected-resource` 和 audience 校验。
- 锁定 `rmcp = "3"`，并订阅 release，因为它大约每周发版。

## 未核实

- Cursor 官方文档（抓取失败）。
- 各客户端当前是否发送 2026-07-28 协议。rmcp 新旧两种协议都支持，所以这一点不影响选型。
- `pmcp`、`turbomcp` 的功能细节。
