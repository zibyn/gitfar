# gitfar

[![CI](https://github.com/zibyn/gitfar/actions/workflows/ci.yml/badge.svg)](https://github.com/zibyn/gitfar/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/zibyn/gitfar)](https://github.com/zibyn/gitfar/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Server-side, read-only code search for remote Git repositories, built for AI agents. Agents use a CLI (or MCP) to search remote code as if it were a local checkout — nothing is cloned on the user's machine.

```console
$ gitfar grep https://github.com/acme/repo 'fn main' src/
src/main.rs:12:fn main() {
```

## Features

- **Familiar primitives.** `read`, `grep`, `list`, `log`, `show`, `blame` mirror `sed -n`, `rg -n`, `ls`/`find` and `git log/show/blame`, with output identical to local git.
- **Reproducible.** Every result carries the resolved commit SHA; results by SHA never change, branches refresh on a TTL.
- **Built for agents.** Bounded output with continuation cursors, stable exit codes, and a remote MCP endpoint for Claude Code and other MCP clients.
- **Self-hosted, single organization.** One `gitfard` instance with a persistent cache, allowlist/denylist, scoped client tokens, and HTTPS credentials for private repos.
- **Operable.** Hot reload on SIGHUP, Prometheus metrics, ready-made alert rules.

## Quick start

Run the server (see [docs/deploy.md](docs/deploy.md) for configuration, TLS and tokens):

```sh
docker pull ghcr.io/zibyn/gitfard:v0.1.1
docker volume create gitfar-cache
docker run -d --name gitfard \
  -v gitfar-cache:/var/lib/gitfar \
  -v "$PWD/gitfard.toml:/etc/gitfar/gitfard.toml:ro" \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:9090:9090 \
  ghcr.io/zibyn/gitfard:v0.1.1
docker run --rm ghcr.io/zibyn/gitfard:v0.1.1 token new --id alice-laptop
```

Start from [`deploy/gitfard.toml`](deploy/gitfard.toml), paste the printed `[[client_tokens]]` entry into it, and `docker kill -s HUP gitfard`.

Install the client: download `gitfar` (linux x86_64) from [Releases](https://github.com/zibyn/gitfar/releases), or `cargo install --locked --git https://github.com/zibyn/gitfar gitfar`.

```sh
export GITFAR_URL=https://gitfar.example.com
export GITFAR_TOKEN=gfr_…
gitfar whoami
```

## Usage

```sh
gitfar read  <repo> <path>          [--rev R] [--start-line N] [--end-line M]
gitfar grep  <repo> <pattern> [path…] [-i] [-F] [-l] [-c] [-A/-B/-C N] [--include G]
gitfar list  <repo> [path]          [--glob G] [--max-depth N] [--type T]
gitfar log   <repo> [path…]         [--max-count N] [--since D] [--author A] [--grep P]
gitfar show  <repo> <rev> [path…]
gitfar blame <repo> <path> --start-line N --end-line M
```

`<repo>` accepts any URL form (`https://…`, `git@host:path`, `host/path`). Truncated results print a cursor; continue with `gitfar <primitive> --cursor <next>`. `--json` prints the full response object. See `gitfar <command> --help` and [spec §5](docs/spec/v1.md#5-cligitfar) for exit codes.

### Claude Code (MCP)

```sh
claude mcp add --transport http gitfar https://gitfar.example.com/mcp \
  --header "Authorization: Bearer gfr_…"
```

## Building from source

Requires a stable Rust toolchain, a C compiler and cmake (TLS uses aws-lc-rs). The server needs git ≥ 2.33 at runtime.

```sh
cargo build --release -p gitfard -p gitfar   # binaries in target/release/
cargo test
```

| Crate | Role |
| --- | --- |
| [`crates/gitfard`](crates/gitfard) | Server: cache, authorization, primitive execution, HTTP API and MCP |
| [`crates/gitfar`](crates/gitfar) | CLI client |
| [`crates/gitfar-proto`](crates/gitfar-proto) | Shared request/response types |

## Documentation

- [Deployment](docs/deploy.md) — image, configuration, TLS, tokens, monitoring
- [Design spec (v1)](docs/spec/v1.md)
- [Architecture decisions](docs/adr/)
- [Glossary](GLOSSARY.md)

## Non-goals (v1)

Remote writes, semantic/vector search, multi-tenant SaaS, multi-instance deployment, and issue/PR metadata search. See [spec §8](docs/spec/v1.md#8-v1-不做).

## License

[MIT](LICENSE)
