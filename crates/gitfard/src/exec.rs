//! 检索执行: the request pipeline shared by HTTP and MCP (spec §1.1 ②–⑧) and its executors (§4.6).

mod blame;
mod cli;
pub mod grep;
mod list;
mod log;
mod read;
mod show;

use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gitfar_proto::{Cursor, ErrorBody, ErrorCode, Params, Primitive, Request, Resume};
use gix::ObjectId;
use tokio::sync::OwnedRwLockReadGuard;

use crate::admin::METRICS;
use crate::authz::{authorize, canonicalize};
use crate::cache::RepoCache;
use crate::config::ConfigHandle;
use crate::http::TokenCtx;

/// Hard cap on the UTF-8 bytes of a response's `text` (spec §2.2).
const BYTE_CAP: usize = 64 * 1024;

/// Runs one primitive request (fresh params or `{cursor}`) for the authenticated `token` and
/// returns the serialized response object. Logs one request line and records request metrics.
pub async fn run(
    config: &ConfigHandle,
    cache: &Arc<RepoCache>,
    token: &TokenCtx,
    primitive: Primitive,
    body: serde_json::Value,
) -> Result<serde_json::Value, ErrorBody> {
    let start = Instant::now();
    let mut log_fields = LogFields::default();
    let res = run_logged(config, cache, token, primitive, body, &mut log_fields).await;
    let elapsed = start.elapsed();
    let (code, commit, reason) = match &res {
        Ok(v) => (
            "ok".to_owned(),
            v["resolved_commit"].as_str().map(str::to_owned),
            v["truncated_reason"].as_str().map(str::to_owned),
        ),
        Err(e) => (
            serde_json::to_value(e.error.code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            None,
            None,
        ),
    };
    let labels = vec![
        ("primitive", primitive.name().to_owned()),
        ("code", code.clone()),
    ];
    METRICS.requests.get_or_create(&labels).inc();
    METRICS
        .duration
        .get_or_create(&labels)
        .observe(elapsed.as_secs_f64());
    if let Some(r) = &reason {
        METRICS
            .truncations
            .get_or_create(&vec![("reason", r.clone())])
            .inc();
    }
    // Only the 规范 URL is logged: the raw `repo` could carry userinfo.
    tracing::info!(
        primitive = primitive.name(),
        repo = log_fields.repo,
        rev = log_fields.rev,
        resolved_commit = commit,
        elapsed_ms = elapsed.as_millis() as u64,
        truncated_reason = reason,
        code,
        token = token.token_id,
        "request"
    );
    res
}

/// What the request log needs from inside the pipeline.
#[derive(Default)]
struct LogFields {
    repo: Option<String>,
    rev: Option<String>,
}

async fn run_logged(
    config: &ConfigHandle,
    cache: &Arc<RepoCache>,
    token: &TokenCtx,
    primitive: Primitive,
    body: serde_json::Value,
    log_fields: &mut LogFields,
) -> Result<serde_json::Value, ErrorBody> {
    let scope = token.scope.as_deref();
    // A fresh request is a resume from offset zero; a cursor carries the SHA it is pinned to.
    let (job, pinned) = match Request::parse(primitive, body)? {
        Request::Fresh(p) => (fresh(p), None),
        Request::Resume(c) => (c.resume, Some(c.resolved_commit)),
    };
    let (repo, rev) = target(&job);
    log_fields.rev = rev.map(str::to_owned);
    let cfg = config.current();
    let deadline = Instant::now() + cfg.server.request_deadline;
    let url = canonicalize(repo).map_err(|e| ErrorBody::new(ErrorCode::InvalidArgument, e))?;
    log_fields.repo = Some(url.clone());
    let cred = authorize(&cfg, scope, &url)
        .map_err(|code| ErrorBody::new(code, "repository not found"))?;
    // Only a fresh rev can be unknown because our mirror is stale; a cursor's SHA was resolved here.
    let mut may_refetch = pinned.is_none();
    let rev = pinned.unwrap_or_else(|| rev.unwrap_or("HEAD").to_owned());
    loop {
        let (path, guard) = cache.ensure_locked(&url, cred, cfg.cache.warm_wait).await?;
        let res = execute(guard, cache, path, rev.clone(), job.clone(), deadline).await;
        match res {
            Err(e) if e.error.code == ErrorCode::RevNotFound && may_refetch => {
                may_refetch = false;
                cache.refetch_for_unknown_rev(&url, cred).await;
            }
            res => return res,
        }
    }
}

/// Resolves `rev` to a commit and runs `job` on it, holding the repo's read lock `guard`.
async fn execute(
    guard: OwnedRwLockReadGuard<()>,
    cache: &Arc<RepoCache>,
    path: PathBuf,
    rev: String,
    job: Resume,
    deadline: Instant,
) -> Result<serde_json::Value, ErrorBody> {
    let git = cache.git().clone();
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        // shortcut: opens the repo per request (cheap, ~ms); keep a ThreadSafeRepository per cached repo if it shows in profiles.
        let repo = gix::open_opts(&path, gix::open::Options::isolated()).map_err(internal)?;
        let git = cli::Runner {
            git,
            dir: path,
            deadline,
        };
        if let Resume::Show {
            params,
            byte_offset,
        } = job
        {
            // `show` resolves its own rev: it may be an `a..b` range.
            return serde_json::to_value(show::run(&repo, &git, params, byte_offset)?)
                .map_err(internal);
        }
        let commit = repo
            .rev_parse_single(rev.as_str())
            .ok()
            .and_then(|id| id.object().ok()?.peel_to_commit().ok())
            .ok_or_else(|| {
                ErrorBody::new(ErrorCode::RevNotFound, format!("unknown revision {rev:?}"))
            })?;
        let value = match job {
            Resume::Read { params, next_line } => {
                serde_json::to_value(read::run(&commit, params, next_line)?)
            }
            Resume::List { params, skipped } => {
                serde_json::to_value(list::run(&commit, params, skipped)?)
            }
            Resume::Log { params, skipped } => {
                serde_json::to_value(log::run(&git, commit.id, params, skipped)?)
            }
            Resume::Blame { params, next_line } => {
                serde_json::to_value(blame::run(&commit, &git, params, next_line)?)
            }
            Resume::Grep { params, path, line } => {
                serde_json::to_value(grep::run(&commit, params, (path, line), deadline)?)
            }
            Resume::Show { .. } => unreachable!("show returned above"),
        };
        value.map_err(internal)
    })
    .await
    .map_err(internal)?
}

/// git CLI children running now and their limit (for `/metrics`).
pub(crate) fn git_processes() -> (usize, usize) {
    let limit = cli::limit();
    (limit - cli::semaphore().available_permits(), limit)
}

fn fresh(p: Params) -> Resume {
    match p {
        Params::Read(p) => Resume::Read {
            next_line: p.start_line.unwrap_or(1),
            params: p,
        },
        Params::List(p) => Resume::List {
            params: p,
            skipped: 0,
        },
        Params::Grep(p) => Resume::Grep {
            params: p,
            path: String::new(),
            line: 0,
        },
        Params::Log(p) => Resume::Log {
            params: p,
            skipped: 0,
        },
        Params::Show(p) => Resume::Show {
            params: p,
            byte_offset: 0,
        },
        Params::Blame(p) => Resume::Blame {
            next_line: p.start_line,
            params: p,
        },
    }
}

/// `(repo, rev)` of a job.
fn target(job: &Resume) -> (&str, Option<&str>) {
    match job {
        Resume::Read { params: p, .. } => (&p.repo, p.rev.as_deref()),
        Resume::Grep { params: p, .. } => (&p.repo, p.rev.as_deref()),
        Resume::List { params: p, .. } => (&p.repo, p.rev.as_deref()),
        Resume::Log { params: p, .. } => (&p.repo, p.rev.as_deref()),
        Resume::Show { params: p, .. } => (&p.repo, Some(&p.rev)),
        Resume::Blame { params: p, .. } => (&p.repo, p.rev.as_deref()),
    }
}

/// The `next` cursor resuming `resume` on `commit`.
fn next(commit: ObjectId, resume: Resume) -> Option<String> {
    Some(
        Cursor {
            resolved_commit: commit.to_string(),
            resume,
        }
        .encode(),
    )
}

/// The tree entry at `path` (`""` or `/` = root), or `path_not_found`.
enum Node<'r> {
    Dir(gix::Tree<'r>),
    Submodule(ObjectId),
    File(gix::Object<'r>),
}

fn lookup<'r>(commit: &gix::Commit<'r>, path: &str) -> Result<Node<'r>, ErrorBody> {
    let tree = commit.tree().map_err(internal)?;
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Ok(Node::Dir(tree));
    }
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(internal)?
        .ok_or_else(|| ErrorBody::new(ErrorCode::PathNotFound, format!("no such path {path:?}")))?;
    let mode = entry.mode();
    Ok(if mode.is_commit() {
        Node::Submodule(entry.object_id())
    } else {
        let obj = entry.object().map_err(internal)?;
        if mode.is_tree() {
            Node::Dir(obj.into_tree())
        } else {
            Node::File(obj)
        }
    })
}

fn invalid(why: impl Display) -> ErrorBody {
    ErrorBody::new(ErrorCode::InvalidArgument, why.to_string())
}

fn internal(e: impl Display) -> ErrorBody {
    tracing::error!(error = %e, "internal error");
    ErrorBody::new(ErrorCode::Internal, "internal error")
}
