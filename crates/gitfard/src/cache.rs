//! 仓库缓存 (spec §4.5): on-disk layout, 预热 through a global FIFO clone queue, per-repo locks.
//!
//! Layout under `cache_dir`: `lock` (single-process file lock), `tmp/<n>` (clones in progress,
//! wiped at startup), `repos/<规范 URL>.git` (finished full bare clones, each with a `gitfar-access`
//! touch file whose mtime is the last access).

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use base64::Engine;
use gitfar_proto::{ErrorBody, ErrorCode, WarmingProgress, WarmingState};
use tokio::process::Command;
use tokio::sync::{watch, RwLock};

use crate::config::Credential;

const TOUCH_FILE: &str = "gitfar-access";

type Outcome = watch::Receiver<Option<Result<(), ErrorCode>>>;

pub struct RepoCache {
    root: PathBuf,
    _lock: File,
    /// Prepended to the 规范 URL to get the clone URL; only tests change it (to `file://…/`).
    base: String,
    git: PathBuf,
    concurrency: usize,
    next_tmp: AtomicU64,
    state: Mutex<State>,
    locks: Mutex<HashMap<String, Arc<RwLock<()>>>>,
}

#[derive(Default)]
struct State {
    /// Every repo queued or cloning, so concurrent requests share one clone.
    warming: HashMap<String, Outcome>,
    queue: VecDeque<Job>,
    running: usize,
}

struct Job {
    url: String,
    cred: Option<Credential>,
    done: watch::Sender<Option<Result<(), ErrorCode>>>,
}

impl RepoCache {
    /// Locks `dir` for this process and discards clones a previous process left half-finished.
    pub fn open(dir: &Path, clone_concurrency: usize) -> io::Result<RepoCache> {
        fs::create_dir_all(dir)?;
        let lock = File::create(dir.join("lock"))?;
        lock.try_lock().map_err(|e| {
            io::Error::other(format!(
                "cache dir {} is in use by another process: {e}",
                dir.display()
            ))
        })?;
        let tmp = dir.join("tmp");
        match fs::remove_dir_all(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        fs::create_dir_all(&tmp)?;
        fs::create_dir_all(dir.join("repos"))?;
        Ok(RepoCache {
            root: dir.to_owned(),
            _lock: lock,
            base: "https://".into(),
            git: "git".into(),
            concurrency: clone_concurrency.max(1),
            next_tmp: AtomicU64::new(0),
            state: Mutex::default(),
            locks: Mutex::default(),
        })
    }

    /// Clone from `<base><规范 URL>.git` using the `git` executable at `git` (tests: `file://` fixtures).
    pub fn with_upstream(mut self, base: &str, git: &Path) -> RepoCache {
        self.base = base.into();
        self.git = git.into();
        self
    }

    /// Where the bare clone of canonical `url` lives (whether or not it is cached yet).
    pub fn path(&self, url: &str) -> PathBuf {
        self.root.join("repos").join(format!("{url}.git"))
    }

    /// The repo's 读写锁: requests and fetch take it shared; gc/repack/淘汰 take it exclusively.
    pub fn lock(&self, url: &str) -> Arc<RwLock<()>> {
        let mut locks = self.locks.lock().unwrap();
        locks.entry(url.to_owned()).or_default().clone()
    }

    /// 预热 without waiting (预热列表 at startup). No-op if cached or already warming.
    pub fn preheat(self: &Arc<Self>, url: &str, cred: Option<&Credential>) {
        self.warm(url, cred);
    }

    /// Returns the cached repo's path, cloning it first if needed and waiting up to `wait`
    /// (`warm_wait`). Times out with `repo_warming` + progress; the clone carries on regardless.
    pub async fn ensure(
        self: &Arc<Self>,
        url: &str,
        cred: Option<&Credential>,
        wait: Duration,
    ) -> Result<PathBuf, ErrorBody> {
        let path = self.path(url);
        if let Some(mut outcome) = self.warm(url, cred) {
            let res = tokio::time::timeout(wait, outcome.wait_for(Option::is_some)).await;
            match res {
                Ok(Ok(done)) => {
                    if let Some(Err(code)) = *done {
                        return Err(ErrorBody::new(code, format!("cloning {url} failed")));
                    }
                }
                Ok(Err(_)) => return Err(ErrorBody::new(ErrorCode::Internal, "clone task lost")),
                Err(_) => return Err(self.warming(url)),
            }
        }
        touch(&path);
        Ok(path)
    }

    /// `None` if `url` is already cached; otherwise the outcome of its (possibly new) clone.
    fn warm(self: &Arc<Self>, url: &str, cred: Option<&Credential>) -> Option<Outcome> {
        let mut st = self.state.lock().unwrap();
        // Checked under the state lock: a finished clone is renamed into place before leaving `warming`.
        if self.path(url).exists() {
            return None;
        }
        if let Some(outcome) = st.warming.get(url) {
            return Some(outcome.clone());
        }
        let (done, outcome) = watch::channel(None);
        st.warming.insert(url.to_owned(), outcome.clone());
        st.queue.push_back(Job {
            url: url.to_owned(),
            cred: cred.cloned(),
            done,
        });
        self.pump(&mut st);
        Some(outcome)
    }

    /// Starts queued clones, FIFO, up to the concurrency limit.
    fn pump(self: &Arc<Self>, st: &mut State) {
        while st.running < self.concurrency {
            let Some(job) = st.queue.pop_front() else {
                return;
            };
            st.running += 1;
            let this = self.clone();
            tokio::spawn(async move {
                let res = this.clone_repo(&job.url, job.cred.as_ref()).await;
                let mut st = this.state.lock().unwrap();
                st.warming.remove(&job.url);
                st.running -= 1;
                this.pump(&mut st);
                job.done.send_replace(Some(res));
            });
        }
    }

    fn warming(&self, url: &str) -> ErrorBody {
        let st = self.state.lock().unwrap();
        let position = st.queue.iter().position(|j| j.url == url);
        let mut body = ErrorBody::new(ErrorCode::RepoWarming, format!("{url} is being cloned"));
        body.error.progress = Some(WarmingProgress {
            state: if position.is_some() {
                WarmingState::Queued
            } else {
                WarmingState::Cloning
            },
            queue_position: position.map(|p| p as u32 + 1),
        });
        body
    }

    async fn clone_repo(&self, url: &str, cred: Option<&Credential>) -> Result<(), ErrorCode> {
        let n = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join("tmp").join(n.to_string());
        let res = self.clone_into(url, cred, &tmp).await;
        if let Err(e) = &res {
            tracing::warn!(url, error = %e, "clone failed");
            let _ = fs::remove_dir_all(&tmp);
        }
        // shortcut: every failure maps by credential presence (spec §4.4); stderr classification,
        // negative cache and cooldown come with the upstream-limits ticket.
        res.map_err(|_| match cred {
            Some(_) => ErrorCode::NotFound,
            None => ErrorCode::UpstreamUnavailable,
        })
    }

    async fn clone_into(
        &self,
        url: &str,
        cred: Option<&Credential>,
        tmp: &Path,
    ) -> Result<(), String> {
        let remote = format!("{}{url}.git", self.base);
        // A bare clone mirrors refs/heads/* and refs/tags/* only, and points HEAD at the upstream default.
        let mut clone = self.git(cred);
        clone
            .args(["clone", "--bare", "-q", "--", &remote])
            .arg(tmp);
        run(clone).await?;
        let mut graph = self.git(None);
        graph
            .arg("-C")
            .arg(tmp)
            .args(["commit-graph", "write", "--reachable", "--changed-paths"]);
        run(graph).await?;
        let dst = self.path(url);
        fs::create_dir_all(dst.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::rename(tmp, &dst).map_err(|e| e.to_string())
    }

    /// A hermetic git child. The credential travels as env-injected `http.<prefix>.extraHeader`
    /// (not `-c`, which shows in /proc/*/cmdline), so it never lands in the repo's config.
    fn git(&self, cred: Option<&Credential>) -> Command {
        let mut c = Command::new(&self.git);
        c.env_clear()
            .envs(std::env::var_os("PATH").map(|p| ("PATH", p)))
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
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

async fn run(mut c: Command) -> Result<(), String> {
    let out = c.output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Records an access for LRU 淘汰; best effort.
fn touch(repo: &Path) {
    let res = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(repo.join(TOUCH_FILE))
        .and_then(|f| f.set_modified(SystemTime::now()));
    if let Err(e) = res {
        tracing::warn!(repo = %repo.display(), error = %e, "touch failed");
    }
}
