//! 检索执行: the request pipeline shared by HTTP and MCP (spec §1.1 ②–⑧) and its executors (§4.6).

mod list;
mod read;

use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;

use gitfar_proto::{Cursor, ErrorBody, ErrorCode, Params, Primitive, Request, Resume};
use gix::ObjectId;

use crate::authz::{authorize, canonicalize};
use crate::cache::RepoCache;
use crate::config::ConfigHandle;

/// Hard cap on the UTF-8 bytes of a response's `text` (spec §2.2).
const BYTE_CAP: usize = 64 * 1024;

/// Runs one primitive request (fresh params or `{cursor}`) for a caller with 令牌范围 `scope`
/// and returns the serialized response object.
pub async fn run(
    config: &ConfigHandle,
    cache: &Arc<RepoCache>,
    scope: Option<&[String]>,
    primitive: Primitive,
    body: serde_json::Value,
) -> Result<serde_json::Value, ErrorBody> {
    // A fresh request is a resume from offset zero; a cursor carries the SHA it is pinned to.
    let (job, pinned) = match Request::parse(primitive, body)? {
        Request::Fresh(p) => (fresh(p), None),
        Request::Resume(c) => (c.resume, Some(c.resolved_commit)),
    };
    let (repo, rev) = target(&job);
    let cfg = config.current();
    let url = canonicalize(repo).map_err(|e| ErrorBody::new(ErrorCode::InvalidArgument, e))?;
    let cred = authorize(&cfg, scope, &url)
        .map_err(|code| ErrorBody::new(code, "repository not found"))?;
    let path = cache.ensure(&url, cred, cfg.cache.warm_wait).await?;
    let resumed = pinned.is_some();
    let rev = pinned.unwrap_or_else(|| rev.unwrap_or("HEAD").to_owned());
    let attempt = || execute(cache, &url, path.clone(), rev.clone(), job.clone());
    match attempt().await {
        // Only a fresh rev can be unknown because our mirror is stale; a cursor's SHA was resolved here.
        Err(e) if e.error.code == ErrorCode::RevNotFound && !resumed => {
            cache.refetch_for_unknown_rev(&url, cred).await;
            attempt().await
        }
        res => res,
    }
}

/// Resolves `rev` to a commit and runs `job` on it, under the repo's read lock.
async fn execute(
    cache: &Arc<RepoCache>,
    url: &str,
    path: PathBuf,
    rev: String,
    job: Resume,
) -> Result<serde_json::Value, ErrorBody> {
    let guard = cache.lock(url).read_owned().await;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        // shortcut: opens the repo per request (cheap, ~ms); keep a ThreadSafeRepository per cached repo if it shows in profiles.
        let repo = gix::open_opts(&path, gix::open::Options::isolated()).map_err(internal)?;
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
            other => {
                return Err(internal(format!(
                    "{} is not implemented",
                    other.primitive().name()
                )))
            }
        };
        value.map_err(internal)
    })
    .await
    .map_err(internal)?
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

/// The `next` token resuming `resume` on `commit`.
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
