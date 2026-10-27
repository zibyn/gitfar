//! git CLI children (ADR 0003): hermetic commands, running them, and classifying failures (spec §4.5).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use tokio::process::Command;

use crate::config::Credential;

/// Builds hermetic git commands: the `git` executable and the base URL prepended to 规范 URLs.
pub struct Git {
    bin: PathBuf,
    /// Prepended to the 规范 URL to get the remote URL; only tests change it (to `file://…/`).
    base: String,
}

impl Git {
    pub fn new(bin: impl Into<PathBuf>, base: impl Into<String>) -> Git {
        Git {
            bin: bin.into(),
            base: base.into(),
        }
    }

    /// The remote URL of 规范 URL `url`.
    pub fn remote(&self, url: &str) -> String {
        format!("{}{url}.git", self.base)
    }

    /// A hermetic git child. The credential travels as env-injected `http.<prefix>.extraHeader`
    /// (not `-c`, which shows in /proc/*/cmdline), so it never lands in the repo's config.
    pub fn command(&self, cred: Option<&Credential>) -> Command {
        let mut c = Command::new(&self.bin);
        c.env_clear()
            .envs(std::env::var_os("PATH").map(|p| ("PATH", p)))
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            // Response headers (for Retry-After) on stderr; `run` strips them from errors.
            .env("GIT_TRACE_CURL", "1")
            .env("GIT_TRACE_CURL_NO_DATA", "1")
            .env(
                "GIT_ALLOW_PROTOCOL",
                self.base.split_once("://").map_or("https", |(s, _)| s),
            )
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(cred) = cred {
            let basic = base64::engine::general_purpose::STANDARD
                .encode(format!("x-access-token:{}", cred.token.expose()));
            c.env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("http.{}{}/.extraHeader", self.base, cred.prefix),
                )
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
        }
        c
    }
}

/// A failed git child: its stderr without the curl trace, classified once.
#[derive(Debug)]
pub struct GitError {
    pub stderr: String,
    pub kind: GitFailure,
}

/// What a failure means upstream-wise; the first match wins, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFailure {
    /// 429/503, with the `Retry-After` received, if any.
    RateLimited {
        retry_after: Option<Duration>,
    },
    NotFound,
    /// 401/403 or a failed credential prompt. GitHub/GitLab answer a bad or revoked token (and
    /// a private repo asked anonymously) with 401, which git turns into a failed username prompt.
    AuthFailed,
    Other,
}

impl GitError {
    /// A failure that is not git's own (spawn errors, I/O, aborts).
    pub fn msg(e: impl ToString) -> GitError {
        GitError {
            stderr: e.to_string(),
            kind: GitFailure::Other,
        }
    }

    /// Classifies raw git stderr as produced under `LC_ALL=C` and `GIT_TRACE_CURL`.
    pub fn from_stderr(raw: &str) -> GitError {
        let mut retry_after = None;
        let mut stderr = Vec::new();
        for line in raw.lines() {
            // GIT_TRACE_CURL lines start with an `HH:MM:SS.micros` timestamp; never keep them.
            let b = line.as_bytes();
            if b.len() > 8 && b[0].is_ascii_digit() && b[2] == b':' && b[5] == b':' {
                let header = line.split_once("<= Recv header: ").map(|(_, h)| h);
                if let Some((name, value)) = header.and_then(|h| h.split_once(':')) {
                    if name.eq_ignore_ascii_case("retry-after") {
                        // shortcut: delta-seconds only; an HTTP-date falls back to backoff. Upgrade if a host sends dates.
                        retry_after = value.trim().parse().ok().map(Duration::from_secs);
                    }
                }
            } else {
                stderr.push(line);
            }
        }
        let stderr = stderr.join("\n").trim().to_owned();
        let has = |s: &[&str]| s.iter().any(|s| stderr.contains(s));
        let kind = if has(&["returned error: 429", "returned error: 503"]) {
            GitFailure::RateLimited { retry_after }
        } else if has(&["returned error: 404"])
            || stderr
                .lines()
                .any(|l| l.starts_with("fatal: repository '") && l.ends_with("' not found"))
        {
            GitFailure::NotFound
        } else if has(&[
            "Authentication failed",
            "could not read Username",
            "could not read Password",
            "returned error: 401",
            "returned error: 403",
        ]) {
            GitFailure::AuthFailed
        } else {
            GitFailure::Other
        };
        GitError { stderr, kind }
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stderr)
    }
}

pub async fn run(c: Command) -> Result<(), GitError> {
    run_out(c).await.map(drop)
}

/// Like [`run`], returning stdout.
pub async fn run_out(mut c: Command) -> Result<Vec<u8>, GitError> {
    let out = c.output().await.map_err(GitError::msg)?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(GitError::from_stderr(&String::from_utf8_lossy(&out.stderr)))
}
