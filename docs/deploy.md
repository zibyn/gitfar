# 部署 gitfard

对应 spec §6。术语见 `GLOSSARY.md`。

## 1. 获取二进制或镜像

**OCI 镜像**只含 `gitfard` 与版本固定的 git（见仓库根目录 `Dockerfile`）：

```sh
docker build -t gitfard .
```

**裸二进制**（`gitfard` 服务端、`gitfar` CLI）：

```sh
cargo build --release -p gitfard -p gitfar   # 产物在 target/release/
```

构建需要 C 编译器和 cmake（`gitfar` 的 TLS 依赖 aws-lc-rs）。运行 `gitfard` 的机器需要 git ≥ 2.33，否则拒绝启动。

## 2. 配置

从 `deploy/gitfard.toml` 起步，完整键说明见 spec §4.1。要点：

- `allowlist`：允许检索的规范 URL 前缀。不在其中的仓库一律 `not_found`。
- `cache_dir` **必须挂持久卷**。它可以重建，但要重新克隆所有仓库，很慢。同一缓存目录只能有一个 `gitfard` 进程（文件锁）。
- `admin_listen` 不鉴权，只能放在内网或只让 Prometheus 访问。镜像示例写 `0.0.0.0:9090` 是为了容器端口映射，裸机部署请用 `127.0.0.1:9090`。
- `public_hosts`：MCP 客户端请求的 `Host` 头（走反向代理时是代理转发过来的 Host）。为空时 `/mcp` 只接受 loopback。
- `allowed_origins`：带 scheme 的 Origin，例如 `https://gitfar.example.com`。不带 `Origin` 的请求（CLI、Claude Code）总是放行。
- 私有仓库凭证写在 `[[credentials]]`，值可为字面值、`env:VAR` 或 `file:/path`。配置文件权限由你负责，建议 `0600` 并纳入版本管理（凭证用 `env:`/`file:` 引用，不进仓库）。

### 用 docker 运行

```sh
docker volume create gitfar-cache
docker run -d --name gitfard \
  -v gitfar-cache:/var/lib/gitfar \
  -v "$PWD/gitfard.toml:/etc/gitfar/gitfard.toml:ro" \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:9090:9090 \
  --stop-timeout 40 \
  gitfard
curl -fsS http://127.0.0.1:9090/readyz
```

镜像以 uid 10001 运行；绑定宿主目录作缓存时先 `chown 10001` 该目录。`--stop-timeout` 要比 `request_deadline` 长，SIGTERM 后进行中的请求最多再跑一个截止时间。

## 3. TLS（反向代理）

`gitfard` 只提供明文 HTTP，TLS 交给反向代理终止。代理要把原始 `Host` 转发过去（与 `public_hosts` 一致），并且不要缓冲响应或设置短于 `request_deadline` 的超时。

Caddy：

```caddyfile
gitfar.example.com {
	reverse_proxy 127.0.0.1:8080
}
```

nginx：

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name gitfar.example.com;
    ssl_certificate     /etc/ssl/gitfar.example.com/fullchain.pem;
    ssl_certificate_key /etc/ssl/gitfar.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_read_timeout 60s;
        proxy_buffering off;
    }
}
```

Kubernetes ingress 同理：TLS 在 ingress 终止，后端指向 8080；管理端口 9090 只暴露给集群内的 Prometheus。

## 4. 热加载（SIGHUP）

```sh
kill -HUP "$(pidof gitfard)"     # 或 docker kill -s HUP gitfard
```

重新加载允许列表、仓库凭证、预热列表、客户端访问令牌、TTL、限速参数、磁盘预算、维护窗口、截止时间。监听地址、`cache_dir`、`scan_threads`、`blob_lru`、`public_hosts`、`allowed_origins` 需要重启。新配置校验失败时保留旧配置，并记一条错误日志，所以每次 SIGHUP 后看一眼日志。

## 5. 客户端访问令牌

**签发**（离线，不改任何文件）：

```sh
gitfard token new --id alice-laptop --scope github.com/acme --expires 2027-01-01T00:00:00Z
# docker：docker run --rm gitfard token new --id alice-laptop
```

令牌原文只打印这一次，交给使用者。把同时打印的 `[[client_tokens]]` 条目粘进配置，然后 SIGHUP。没有任何令牌时所有请求都是 401。

**轮换**：新增条目（新 `id`）→ SIGHUP → 客户端换成新令牌 → 删除旧条目 → SIGHUP。并存期间新旧令牌都有效。

**吊销**：删除条目，或把 `expires_at` 改成过去的时间，然后 SIGHUP。热加载成功后每个请求（含续读）立即被拒；进行中的请求最多再跑一个截止时间。日志里按令牌标识（`id`）追查，令牌原文绝不进日志。

## 6. 缓存丢失

不做备份。缓存目录丢失或换新卷后，预热列表中的仓库会在启动时**全部重新克隆**，其余仓库在首次请求时克隆。预热列表很大时会对上游形成一次流量高峰，可能触发上游限速（冷却）。可以先缩小 `warm_list` 启动，再分批加回并 SIGHUP。

## 7. 监控与告警

管理监听地址提供 `/metrics`（OpenMetrics）、`/status`（JSON）、`/healthz`、`/readyz`。`/readyz` 只检查配置已加载、缓存目录可写、git 可用；上游冷却与 `cache_full` 不影响就绪。

推荐告警规则在 `deploy/alerts.yml`（`promtool check rules deploy/alerts.yml`），覆盖 spec §6.5：`cache_full` 持续、host 冷却超过 30 分钟、预热列表超出磁盘预算、预热仓库连续 fetch 失败、克隆队列积压、空闲磁盘逼近硬下限。阈值按部署调整。

```yaml
# prometheus.yml
rule_files: [alerts.yml]
scrape_configs:
  - job_name: gitfard
    static_configs: [{ targets: ["127.0.0.1:9090"] }]
```

## 8. 接入客户端

CLI：

```sh
export GITFAR_URL=https://gitfar.example.com
export GITFAR_TOKEN=gfr_…
gitfar whoami
gitfar grep https://github.com/acme/repo 'fn main'
```

也可以写进 `~/.config/gitfar/config.toml`（`url`、`token` 两个键）。

Claude Code（MCP）：

```sh
claude mcp add --transport http gitfar https://gitfar.example.com/mcp \
  --header "Authorization: Bearer gfr_…"
```

然后让 Claude 在 `https://github.com/acme/repo` 里 grep 一个符号。首次请求某仓库可能返回 `repo_warming`（正在克隆），稍后重试即可。
