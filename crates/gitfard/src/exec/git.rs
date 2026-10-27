//! git CLI executor for `log`/`show`/`blame` (spec §4.6): global semaphore, deadline, output cap.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use gitfar_proto::{ErrorBody, ErrorCode};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

use super::{internal, invalid};
use crate::git::Git;

/// git children running at once, across all requests (spec: default = cores).
pub(super) fn semaphore() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(limit()))
}

pub(super) fn limit() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Runs git in one cached repo. Callers pass only resolved 40-hex SHAs as revs and put every
/// user-supplied path after `--`; pathspecs are literal, so `:(magic)` is just a name.
pub(super) struct Runner {
    pub git: Arc<Git>,
    pub dir: PathBuf,
    pub deadline: Instant,
}

impl Runner {
    /// stdout of `git <args>`, reading at most `limit` bytes (then the child is killed).
    pub fn run(&self, args: &[String], limit: usize) -> Result<Vec<u8>, ErrorBody> {
        let mut cmd = self.git.command(None);
        cmd.arg("--literal-pathspecs")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let deadline = self.deadline;
        let fut = async move {
            let _permit = semaphore().acquire().await.map_err(internal)?;
            // Never start git past the deadline: a fast git could otherwise beat the timeout.
            if Instant::now() >= deadline {
                return Err(deadline_exceeded());
            }
            let mut child = cmd.spawn().map_err(internal)?;
            // Drained alongside stdout, so a chatty git never blocks on a full stderr pipe.
            let mut stderr = child.stderr.take().expect("piped");
            let stderr = tokio::spawn(async move {
                let mut buf = Vec::new();
                let _ = stderr.read_to_end(&mut buf).await;
                buf
            });
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
            let status = child.wait().await.map_err(internal)?;
            if status.success() {
                return Ok(out);
            }
            let stderr = stderr.await.map_err(internal)?;
            let err = String::from_utf8_lossy(&stderr);
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
            tokio::time::timeout_at(deadline.into(), fut)
                .await
                .unwrap_or_else(|_| Err(deadline_exceeded()))
        })
    }
}

fn deadline_exceeded() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::DeadlineExceeded,
        "git did not finish within the request deadline",
    )
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
