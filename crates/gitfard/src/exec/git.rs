//! git CLI executor for `log`/`show`/`blame` (spec §4.6): global semaphore, deadline, hermetic env.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gitfar_proto::{ErrorBody, ErrorCode};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

use super::{internal, invalid};
use crate::cache::RepoCache;

/// git children running at once, across all requests (spec: default = cores).
pub(super) fn semaphore() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(std::thread::available_parallelism().map_or(4, |n| n.get())))
}

/// Runs git in one cached repo. Callers pass only resolved 40-hex SHAs as revs and put every
/// user-supplied path after `--`; pathspecs are literal, so `:(magic)` is just a name.
pub(super) struct Git {
    pub cache: Arc<RepoCache>,
    pub dir: PathBuf,
    pub deadline: Duration,
}

impl Git {
    /// stdout of `git <args>`, reading at most `limit` bytes (then the child is killed).
    pub fn run(&self, args: &[String], limit: usize) -> Result<Vec<u8>, ErrorBody> {
        let mut cmd = self.cache.git(None);
        cmd.arg("--literal-pathspecs")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let fut = async move {
            let _permit = semaphore().acquire().await.map_err(internal)?;
            let mut child = cmd.spawn().map_err(internal)?;
            let mut out = Vec::new();
            let stdout = child.stdout.take().expect("piped");
            stdout
                .take(limit as u64)
                .read_to_end(&mut out)
                .await
                .map_err(internal)?;
            if out.len() == limit {
                // The caller has all it can use; the rest may be large.
                return Ok(out);
            }
            let done = child.wait_with_output().await.map_err(internal)?;
            if done.status.success() {
                return Ok(out);
            }
            let err = String::from_utf8_lossy(&done.stderr);
            let err = err.trim();
            // Bad user input git rejects itself, e.g. an invalid `--grep` regex.
            if err.starts_with("fatal: command line") {
                Err(invalid(err))
            } else {
                Err(internal(format!("git {args:?}: {err}")))
            }
        };
        // The child is `kill_on_drop`, so a timeout kills it.
        tokio::runtime::Handle::current().block_on(async {
            tokio::time::timeout(self.deadline, fut)
                .await
                .unwrap_or_else(|_| {
                    Err(ErrorBody::new(
                        ErrorCode::DeadlineExceeded,
                        "git did not finish within the request deadline",
                    ))
                })
        })
    }
}

/// User path prefixes as literal pathspecs; `""`/`/` (the root) is dropped since git rejects it.
pub(super) fn pathspecs(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}
