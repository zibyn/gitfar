//! 仓库缓存 (spec §4.5): on-disk layout, 预热 through a global FIFO clone queue, per-repo locks.
//!
//! Layout under `cache_dir`: `lock` (single-process file lock), `tmp/<n>` (clones in progress,
//! wiped at startup), `repos/<规范 URL>.git` (finished full bare clones, each with a `gitfar-access`
//! touch file whose mtime is the last access, and a `gitfar-gc` file whose mtime is the last full gc).
//!
//! The cache reads its config live (SIGHUP applies) and enforces `disk_budget` (LRU 淘汰), `disk_min_free`
//! (`cache_full`) and runs the daily maintenance window from [`RepoCache::tick`].
//!
//! 上游限速 (§4.5, [`crate::upstream`]): clones and fetches take a per-host slot; a 429/503 puts the whole host in
//! 冷却 (due fetches skipped, queued clones held); deterministic clone failures are 负缓存'd.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gitfar_proto::{ErrorBody, ErrorCode, WarmingProgress, WarmingState};
use tokio::sync::{watch, OwnedRwLockReadGuard, RwLock};
use tokio::time::timeout_at;

use crate::admin::METRICS;
use crate::authz::authorize;
use crate::config::{Config, ConfigHandle, Credential};
use crate::git::{run, run_out, Git, GitError, GitFailure};
use crate::upstream::UpstreamLimits;

const TOUCH_FILE: &str = "gitfar-access";
/// An unresolvable rev triggers a fetch only if the last successful one is older than this (§4.5).
const REV_REFETCH_AFTER: Duration = Duration::from_secs(60);
/// More packs than this after a fetch triggers a geometric repack (§4.5 维护).
const MAX_PACKS: usize = 16;
const GC_FILE: &str = "gitfar-gc";
/// A repo is due for full gc once per maintenance window; anything shorter than a day works.
const GC_EVERY: Duration = Duration::from_secs(12 * 3600);
/// How often a running clone checks free disk and the budget.
const CLONE_POLL: Duration = Duration::from_millis(500);

type Outcome = watch::Receiver<Option<Result<(), ErrorCode>>>;

pub struct RepoCache {
    root: PathBuf,
    _lock: File,
    git: Arc<Git>,
    next_tmp: AtomicU64,
    state: Mutex<State>,
    locks: Mutex<HashMap<String, Arc<RwLock<()>>>>,
    fresh: Mutex<HashMap<String, Freshness>>,
    config: Arc<ConfigHandle>,
    /// One 淘汰 pass at a time, so concurrent passes don't each evict for the same overrun.
    evicting: tokio::sync::Mutex<()>,
    upstream: UpstreamLimits,
}

/// Per-repo refresh state; in memory only, so after a restart the first request fetches.
#[derive(Default)]
struct Freshness {
    /// Last fetch (or clone) that finished, successfully or not: the branch TTL counts from here.
    checked: Option<Instant>,
    /// Last successful fetch (or clone).
    fetched: Option<Instant>,
    /// The singleflight fetch in progress: when it started, and a flag flipped when it ends.
    running: Option<(Instant, watch::Receiver<bool>)>,
    /// 凭证失效: the last fetch failed authentication.
    cred_invalid: bool,
}

#[derive(Default)]
struct State {
    /// Every repo queued or cloning, so concurrent requests share one clone.
    warming: HashMap<String, Outcome>,
    queue: VecDeque<Job>,
    running: usize,
    /// 负缓存: deterministic clone failures and when they expire.
    negative: HashMap<String, (Instant, ErrorCode)>,
}

struct Job {
    url: String,
    cred: Option<Credential>,
    done: watch::Sender<Option<Result<(), ErrorCode>>>,
}

impl RepoCache {
    /// Locks `server.cache_dir` for this process and discards clones a previous process left
    /// half-finished. Reads `[cache]`, `[upstream]`, `warm_list`, `allowlist` and `denylist` live.
    pub fn open(config: Arc<ConfigHandle>) -> io::Result<RepoCache> {
        let dir = &config.current().server.cache_dir;
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
            git: Arc::new(Git::new("git", "https://")),
            next_tmp: AtomicU64::new(0),
            state: Mutex::default(),
            locks: Mutex::default(),
            fresh: Mutex::default(),
            upstream: UpstreamLimits::new(config.clone()),
            config,
            evicting: tokio::sync::Mutex::default(),
        })
    }

    /// Periodic housekeeping (main calls it every minute with the local minute of day): 淘汰 down
    /// to budget, resume clones paused for low disk, and inside the maintenance window full gc
    /// of due repos plus removal of repos no longer authorized.
    pub async fn tick(self: &Arc<Self>, now_minute: u16) {
        let cfg = self.config();
        self.evict().await;
        self.pump(&mut self.state.lock().unwrap());
        if cfg.cache.maintenance_window.contains(now_minute) {
            self.maintain(&cfg).await;
        }
    }

    /// Clone from `<base><规范 URL>.git` using the `git` executable at `git` (tests: `file://` fixtures).
    pub fn with_upstream(mut self, base: &str, git: &Path) -> RepoCache {
        self.git = Arc::new(Git::new(git, base));
        self
    }

    /// The hermetic git command builder clones and fetches use.
    pub fn git(&self) -> &Arc<Git> {
        &self.git
    }

    /// Hosts in 冷却 and the time left, by host (for the admin port).
    pub fn cooldowns(&self) -> Vec<(String, Duration)> {
        self.upstream.snapshot(Instant::now())
    }

    /// Every repo cloning or queued: cloning first, then in queue order (for the admin port).
    pub fn clone_queue(&self) -> Vec<(String, WarmingProgress)> {
        let st = self.state.lock().unwrap();
        let mut v: Vec<_> = st
            .warming
            .keys()
            .filter(|u| !st.queue.iter().any(|j| &j.url == *u))
            .map(|u| (u.clone(), self.progress(&st, u)))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.extend(
            st.queue
                .iter()
                .map(|j| (j.url.clone(), self.progress(&st, &j.url))),
        );
        v
    }

    /// Disk usage against the budget and floor. Walks the cache dir.
    pub fn disk(&self) -> DiskStatus {
        let cfg = self.config();
        let repos = self.repos();
        let size = |url: &String| dir_size(&self.path(url));
        let used = dir_size(&self.root.join("tmp")) + repos.iter().map(size).sum::<u64>();
        let warm: u64 = repos
            .iter()
            .filter(|u| cfg.warm_list.contains(u))
            .map(size)
            .sum();
        let free =
            rustix::fs::statvfs(&self.root).map_or(0, |s| s.f_bavail.saturating_mul(s.f_frsize));
        DiskStatus {
            used,
            budget: cfg.cache.disk_budget,
            free,
            min_free: cfg.cache.disk_min_free,
            cache_full: self.disk_low(),
            warm_list_bytes: warm,
            warm_list_over_budget: warm > cfg.cache.disk_budget,
        }
    }

    /// The cache dir accepts new files (for `/readyz`).
    pub fn writable(&self) -> bool {
        let probe = self.root.join("tmp").join("readyz");
        let ok = File::create(&probe).is_ok();
        let _ = fs::remove_file(&probe);
        ok
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

    /// 预热列表 upkeep (main calls it at startup and every branch TTL): 预热s entries not cached
    /// yet, e.g. added by a reload, and refreshes the rest in the background.
    pub fn refresh_warm_list(self: &Arc<Self>) {
        let cfg = self.config();
        for url in &cfg.warm_list {
            let Ok(cred) = authorize(&cfg, None, url) else {
                continue;
            };
            if self.warm(url, cred).is_none() {
                let (this, url, cred) = (self.clone(), url.clone(), cred.cloned());
                tokio::spawn(async move { this.refresh(&url, cred.as_ref()).await });
            }
        }
    }

    /// Returns the cached repo's path, cloning it first if needed and waiting up to `wait`
    /// (`warm_wait`). Times out with `repo_warming` + progress; the clone carries on regardless.
    /// A cached repo past its branch TTL is fetched first (see [`RepoCache::refresh`]); a repo
    /// marked 凭证失效 answers `upstream_unavailable`.
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
        self.refresh(url, cred).await;
        if self.fresh_state(url, |f| f.cred_invalid) {
            return Err(ErrorBody::new(
                ErrorCode::UpstreamUnavailable,
                format!("upstream rejected the credential for {url}"),
            ));
        }
        touch(&path.join(TOUCH_FILE));
        Ok(path)
    }

    /// [`RepoCache::ensure`], then the repo's shared lock, held by the caller while it reads.
    /// A repo 淘汰'd in between is 预热ed again, within the same `wait`.
    pub async fn ensure_locked(
        self: &Arc<Self>,
        url: &str,
        cred: Option<&Credential>,
        wait: Duration,
    ) -> Result<(PathBuf, OwnedRwLockReadGuard<()>), ErrorBody> {
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let path = self.ensure(url, cred, left).await?;
            let read = self.lock(url).read_owned().await;
            if path.exists() {
                return Ok((path, read));
            }
        }
    }

    /// Fetches a cached repo whose branch TTL has expired, waiting at most `fetch_timeout` from
    /// the fetch's start; on timeout or failure the old refs stay and the fetch carries on.
    /// Concurrent callers share one fetch. Also drives background refresh of the 预热列表.
    pub async fn refresh(self: &Arc<Self>, url: &str, cred: Option<&Credential>) {
        let ttl = self.branch_ttl();
        self.fetch_if(url, cred, |f| f.checked.is_none_or(|t| t.elapsed() >= ttl))
            .await;
    }

    /// For a rev that did not resolve: fetches (as [`RepoCache::refresh`]) unless the last
    /// successful fetch is under 60 s old. The caller then retries the resolution once.
    pub async fn refetch_for_unknown_rev(self: &Arc<Self>, url: &str, cred: Option<&Credential>) {
        self.fetch_if(url, cred, |f| {
            f.fetched.is_none_or(|t| t.elapsed() >= REV_REFETCH_AFTER)
        })
        .await;
    }

    fn fresh_state<T>(&self, url: &str, f: impl FnOnce(&mut Freshness) -> T) -> T {
        f(self
            .fresh
            .lock()
            .unwrap()
            .entry(url.to_owned())
            .or_default())
    }

    async fn fetch_if(
        self: &Arc<Self>,
        url: &str,
        cred: Option<&Credential>,
        due: impl FnOnce(&Freshness) -> bool,
    ) {
        if !self.path(url).exists() {
            return;
        }
        let cooling = self.upstream.cooling(host(url), Instant::now());
        let running = self.fresh_state(url, |f| {
            if f.running.is_none() && !cooling && due(f) {
                let (tx, rx) = watch::channel(false);
                f.running = Some((Instant::now(), rx));
                let (this, url, cred) = (self.clone(), url.to_owned(), cred.cloned());
                tokio::spawn(async move {
                    this.fetch(&url, cred.as_ref()).await;
                    tx.send_replace(true);
                });
            }
            f.running.clone()
        });
        if let Some((started, mut done)) = running {
            let deadline = started + self.fetch_timeout();
            let _ = timeout_at(deadline.into(), done.wait_for(|d| *d)).await;
        }
    }

    async fn fetch(self: &Arc<Self>, url: &str, cred: Option<&Credential>) {
        let path = self.path(url);
        let lock = self.lock(url);
        let res = {
            let _read = lock.read().await;
            // shortcut: a fetch already waiting for a slot still runs if the host starts cooling
            // meanwhile; re-check after acquiring if bursts after a 429 become a problem.
            let _slot = self.upstream.slot(host(url)).await;
            let remote = self.git.remote(url);
            let mut fetch = self.git.command(cred);
            fetch.arg("-C").arg(&path).args([
                "fetch",
                "-q",
                "--prune",
                "--no-write-fetch-head",
                "--",
                &remote,
                // A bare clone has no fetch refspec of its own.
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ]);
            let res = run(fetch).await;
            if res.is_ok() {
                self.follow_head(&path, &remote, cred).await;
            }
            self.record(url, &res);
            let ok = if res.is_ok() { "ok" } else { "error" };
            METRICS
                .fetches
                .get_or_create(&vec![("result", ok.into())])
                .inc();
            if self.config().warm_list.iter().any(|w| w == url) {
                let g = METRICS
                    .warm_fetch_failures
                    .get_or_create(&vec![("repo", url.into())]);
                if res.is_ok() {
                    g.set(0);
                } else {
                    g.inc();
                }
            }
            match res {
                Ok(()) => self.write_commit_graph(&path).await,
                Err(e) => Err(e),
            }
        };
        if res.is_ok() && count_packs(&path) > MAX_PACKS {
            let _write = lock.write().await;
            let mut repack = self.git.command(None);
            repack
                .arg("-C")
                .arg(&path)
                .args(["repack", "-q", "--geometric=2", "-d"]);
            let res = match run(repack).await {
                Ok(()) => self.write_commit_graph(&path).await,
                Err(e) => Err(e),
            };
            if let Err(e) = res {
                tracing::warn!(url, error = %e, "repack failed");
            }
        }
        let auth_failed = matches!(
            res,
            Err(GitError {
                kind: GitFailure::AuthFailed,
                ..
            })
        );
        match &res {
            Ok(()) => tracing::info!(url, "fetched"),
            Err(e) => tracing::warn!(url, error = %e, auth_failed, "fetch failed"),
        }
        self.fresh_state(url, |f| {
            let now = Instant::now();
            f.checked = Some(now);
            f.running = None;
            if res.is_ok() {
                f.fetched = Some(now);
                f.cred_invalid = false;
            } else if auth_failed {
                f.cred_invalid = true;
            }
        });
    }

    /// Points HEAD at the upstream default branch, which a fetch alone never moves (§4.5).
    /// Best effort: on failure HEAD stays as it was.
    async fn follow_head(&self, repo: &Path, remote: &str, cred: Option<&Credential>) {
        let mut ls = self.git.command(cred);
        ls.args(["ls-remote", "--symref", "--", remote, "HEAD"]);
        let out = match run_out(ls).await {
            Ok(out) => out,
            Err(e) => return tracing::warn!(remote, error = %e, "reading upstream HEAD failed"),
        };
        let out = String::from_utf8_lossy(&out);
        let Some(head) = out.lines().find_map(|l| {
            l.strip_prefix("ref: ")?
                .strip_suffix("\tHEAD")
                .filter(|r| r.starts_with("refs/heads/"))
        }) else {
            return;
        };
        let mut set = self.git.command(None);
        set.arg("-C").arg(repo).args(["symbolic-ref", "HEAD", head]);
        if let Err(e) = run(set).await {
            tracing::warn!(remote, head, error = %e, "updating HEAD failed");
        }
    }

    async fn write_commit_graph(&self, repo: &Path) -> Result<(), GitError> {
        let mut graph = self.git.command(None);
        graph.arg("-C").arg(repo).args([
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--split",
        ]);
        run(graph).await
    }

    /// `None` if `url` is already cached; otherwise the outcome of its (possibly new) clone.
    fn warm(self: &Arc<Self>, url: &str, cred: Option<&Credential>) -> Option<Outcome> {
        let mut st = self.state.lock().unwrap();
        // Checked under the state lock: a finished clone is renamed into place before leaving `warming`.
        if self.path(url).exists() {
            return None;
        }
        match st.negative.get(url) {
            Some(&(until, code)) if until > Instant::now() => {
                return Some(watch::channel(Some(Err(code))).1);
            }
            Some(_) => {
                st.negative.remove(url);
            }
            None => {}
        }
        if let Some(outcome) = st.warming.get(url) {
            return Some(outcome.clone());
        }
        if self.disk_low() {
            return Some(watch::channel(Some(Err(ErrorCode::CacheFull))).1);
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

    /// Starts queued clones, FIFO, up to the concurrency limit. Paused while disk is low; `tick` resumes.
    fn pump(self: &Arc<Self>, st: &mut State) {
        // Read each time, so a reloaded clone_concurrency applies to clones already queued.
        let limit = self.config().cache.clone_concurrency.max(1);
        while st.running < limit && !self.disk_low() {
            // Clones to a cooling host stay queued; the cooldown's end pumps again.
            let now = Instant::now();
            let Some(i) = st
                .queue
                .iter()
                .position(|j| !self.upstream.cooling(host(&j.url), now))
            else {
                return;
            };
            let job = st.queue.remove(i).unwrap();
            st.running += 1;
            let this = self.clone();
            tokio::spawn(async move {
                let res = this.clone_repo(&job.url, job.cred.as_ref()).await;
                let ok = if res.is_ok() { "ok" } else { "error" };
                METRICS
                    .clones
                    .get_or_create(&vec![("result", ok.into())])
                    .inc();
                let res = match res {
                    Ok(()) => {
                        tracing::info!(url = job.url, "cloned");
                        Ok(())
                    }
                    Err(Failed::RateLimited) => {
                        let mut st = this.state.lock().unwrap();
                        st.running -= 1;
                        st.queue.push_front(job);
                        this.pump(&mut st);
                        return;
                    }
                    Err(Failed::Final(code, negative)) => {
                        if negative {
                            let until = Instant::now() + this.config().upstream.negative_cache_ttl;
                            let mut st = this.state.lock().unwrap();
                            st.negative.insert(job.url.clone(), (until, code));
                        }
                        Err(code)
                    }
                };
                if res.is_ok() {
                    // A fresh clone counts as a successful fetch.
                    let now = Instant::now();
                    this.fresh_state(&job.url, |f| {
                        *f = Freshness {
                            checked: Some(now),
                            fetched: Some(now),
                            ..Freshness::default()
                        }
                    });
                }
                {
                    let mut st = this.state.lock().unwrap();
                    st.warming.remove(&job.url);
                    st.running -= 1;
                    this.pump(&mut st);
                }
                job.done.send_replace(Some(res));
                this.evict().await;
            });
        }
    }

    fn warming(&self, url: &str) -> ErrorBody {
        let st = self.state.lock().unwrap();
        let mut body = ErrorBody::new(ErrorCode::RepoWarming, format!("{url} is being cloned"));
        let progress = self.progress(&st, url);
        if progress.state == WarmingState::RateLimited {
            let now = Instant::now();
            let until = self.upstream.cool_until(host(url), now);
            body.retry_after = until.map(|t| t - now);
        }
        body.error.progress = Some(progress);
        body
    }

    fn progress(&self, st: &State, url: &str) -> WarmingProgress {
        let position = st.queue.iter().position(|j| j.url == url);
        WarmingProgress {
            state: match position {
                None => WarmingState::Cloning,
                Some(_) if self.upstream.cooling(host(url), Instant::now()) => {
                    WarmingState::RateLimited
                }
                Some(_) => WarmingState::Queued,
            },
            queue_position: position.map(|p| p as u32 + 1),
        }
    }

    fn branch_ttl(&self) -> Duration {
        self.config().cache.branch_ttl
    }

    fn fetch_timeout(&self) -> Duration {
        self.config().cache.fetch_timeout
    }

    /// Feeds a clone or fetch result into 上游限速; when it starts a 冷却, queued clones to the
    /// host wait for its end, which pumps the queue again.
    fn record(self: &Arc<Self>, url: &str, res: &Result<(), GitError>) {
        let now = Instant::now();
        let res = res.as_ref().map(drop).map_err(|e| e.kind);
        let Some(until) = self.upstream.record(host(url), res, now) else {
            return;
        };
        tracing::warn!(host = host(url), cooldown = ?until - now, "upstream rate limited, host cooling");
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(until.into()).await;
            let mut st = this.state.lock().unwrap();
            this.pump(&mut st);
        });
    }

    async fn clone_repo(
        self: &Arc<Self>,
        url: &str,
        cred: Option<&Credential>,
    ) -> Result<(), Failed> {
        let n = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join("tmp").join(n.to_string());
        let Err(e) = self.clone_into(url, cred, &tmp).await else {
            return Ok(());
        };
        tracing::warn!(url, error = %e, "clone failed");
        let _ = fs::remove_dir_all(&tmp);
        if self.disk_low() {
            return Err(Failed::Final(ErrorCode::CacheFull, false));
        }
        Err(match e.kind {
            GitFailure::RateLimited { .. } => Failed::RateLimited,
            GitFailure::NotFound => Failed::Final(ErrorCode::NotFound, true),
            // Anonymous: the repo may be private and need a credential (§4.4). GitHub/GitLab
            // answer 401 for missing repos too, so either way it is deterministic.
            GitFailure::AuthFailed if cred.is_some() => Failed::Final(ErrorCode::NotFound, true),
            GitFailure::AuthFailed => Failed::Final(ErrorCode::UpstreamUnavailable, true),
            // Network and other errors: neither negatively cached nor cooled.
            GitFailure::Other => Failed::Final(ErrorCode::UpstreamUnavailable, false),
        })
    }

    async fn clone_into(
        self: &Arc<Self>,
        url: &str,
        cred: Option<&Credential>,
        tmp: &Path,
    ) -> Result<(), GitError> {
        let remote = self.git.remote(url);
        // A bare clone mirrors refs/heads/* and refs/tags/* only, and points HEAD at the upstream default.
        let mut clone = self.git.command(cred);
        clone
            .args(["clone", "--bare", "-q", "--", &remote])
            .arg(tmp);
        let res = {
            let _slot = self.upstream.slot(host(url)).await;
            let out = run(clone);
            tokio::pin!(out);
            loop {
                tokio::select! {
                    res = &mut out => break res,
                    _ = tokio::time::sleep(CLONE_POLL) => {
                        // Dropping `out` kills the clone (kill_on_drop); the caller removes `tmp`.
                        // shortcut: git's own children (index-pack) may outlive the kill briefly; kill the
                        // process group if residue shows up after an abort.
                        if self.disk_low() {
                            return Err(GitError::msg("free disk below disk_min_free"));
                        }
                        self.evict().await;
                    }
                }
            }
        };
        self.record(url, &res);
        res?;
        let mut graph = self.git.command(None);
        graph
            .arg("-C")
            .arg(tmp)
            .args(["commit-graph", "write", "--reachable", "--changed-paths"]);
        run(graph).await?;
        touch(&tmp.join(TOUCH_FILE));
        let dst = self.path(url);
        fs::create_dir_all(dst.parent().unwrap()).map_err(GitError::msg)?;
        fs::rename(tmp, &dst).map_err(GitError::msg)
    }

    fn config(&self) -> Arc<Config> {
        self.config.current()
    }

    /// Free disk under the cache dir is below `disk_min_free`.
    fn disk_low(&self) -> bool {
        match rustix::fs::statvfs(&self.root) {
            Ok(s) => s.f_bavail.saturating_mul(s.f_frsize) < self.config().cache.disk_min_free,
            Err(e) => {
                tracing::warn!(error = %e, "statvfs failed");
                false
            }
        }
    }

    /// Canonical URLs of every cached repo.
    fn repos(&self) -> Vec<String> {
        fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
            for e in fs::read_dir(dir).into_iter().flatten().flatten() {
                let url = format!("{prefix}{}", e.file_name().to_string_lossy());
                match url.strip_suffix(".git") {
                    Some(repo) => out.push(repo.to_owned()),
                    None if e.file_type().is_ok_and(|t| t.is_dir()) => {
                        walk(&e.path(), &format!("{url}/"), out)
                    }
                    None => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root.join("repos"), "", &mut out);
        out
    }

    /// LRU 淘汰 of whole repos until `repos/` + in-progress clones fit `disk_budget`. 预热列表 repos stay.
    async fn evict(&self) {
        let cfg = self.config();
        let _one = self.evicting.lock().await;
        let mut repos: Vec<_> = self
            .repos()
            .into_iter()
            .map(|url| {
                let path = self.path(&url);
                let used = mtime(&path.join(TOUCH_FILE)).or_else(|| mtime(&path));
                (used, dir_size(&path), url)
            })
            .collect();
        let budget = cfg.cache.disk_budget;
        let mut total = dir_size(&self.root.join("tmp")) + repos.iter().map(|r| r.1).sum::<u64>();
        if total <= budget {
            return;
        }
        repos.sort();
        for (_, size, url) in repos {
            if total <= budget {
                return;
            }
            if !cfg.warm_list.contains(&url) {
                tracing::info!(url, size, "evicting");
                METRICS.evictions.inc();
                self.remove(&url).await;
                total -= size;
            }
        }
        tracing::warn!(total, budget, "warm_list repos alone exceed disk_budget");
    }

    /// Full gc of due repos and removal of repos no longer authorized (spec §4.5 维护).
    async fn maintain(&self, cfg: &Config) {
        for url in self.repos() {
            if authorize(cfg, None, &url).is_err() {
                tracing::info!(url, "removing repo no longer authorized");
                self.remove(&url).await;
                continue;
            }
            let path = self.path(&url);
            let marker = path.join(GC_FILE);
            if mtime(&marker).is_some_and(|t| t.elapsed().is_ok_and(|e| e < GC_EVERY)) {
                continue;
            }
            let lock = self.lock(&url);
            let _w = lock.write().await;
            let mut gc = self.git.command(None);
            gc.arg("-C").arg(&path).args(["gc", "-q", "--prune=now"]);
            let mut graph = self.git.command(None);
            graph.arg("-C").arg(&path).args([
                "commit-graph",
                "write",
                "--reachable",
                "--changed-paths",
            ]);
            let res = match run(gc).await {
                Ok(()) => run(graph).await,
                e => e,
            };
            match res {
                Ok(()) => {
                    tracing::info!(url, "gc finished");
                    touch(&marker)
                }
                Err(e) => tracing::warn!(url, error = %e, "gc failed"),
            }
        }
    }

    /// Deletes a cached repo under its write lock, dropping its lock entry if no one else holds it.
    async fn remove(&self, url: &str) {
        let lock = self.lock(url);
        let w = lock.write().await;
        // Renamed out of `repos/` first, so a half-deleted repo never looks cached.
        let n = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        let trash = self.root.join("tmp").join(format!("rm-{n}"));
        let res = fs::rename(self.path(url), &trash).and_then(|_| fs::remove_dir_all(&trash));
        if let Err(e) = res {
            tracing::warn!(url, error = %e, "removing repo failed");
        }
        drop(w);
        let mut locks = self.locks.lock().unwrap();
        // Only the map and `lock` hold it; anyone else (even still waiting) keeps it shared.
        if Arc::strong_count(&lock) == 2 {
            locks.remove(url);
        }
    }
}

/// Disk state for `/status` and `/metrics`.
#[derive(Debug, serde::Serialize)]
pub struct DiskStatus {
    pub used: u64,
    pub budget: u64,
    pub free: u64,
    pub min_free: u64,
    pub cache_full: bool,
    pub warm_list_bytes: u64,
    pub warm_list_over_budget: bool,
}

/// Why a clone failed, as the clone queue acts on it.
enum Failed {
    /// Back in the queue until the host's 冷却 ends.
    RateLimited,
    /// Reported to waiters; `true` = negatively cached.
    Final(ErrorCode, bool),
}

/// The host part of a 规范 URL.
fn host(url: &str) -> &str {
    url.split('/').next().unwrap_or(url)
}

fn count_packs(repo: &Path) -> usize {
    fs::read_dir(repo.join("objects/pack")).map_or(0, |d| {
        d.filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "pack"))
        })
        .count()
    })
}

/// Sets `file`'s mtime to now (last access for LRU 淘汰, last full gc); best effort.
fn touch(file: &Path) {
    let res = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file)
        .and_then(|f| f.set_modified(SystemTime::now()));
    if let Err(e) = res {
        tracing::warn!(file = %file.display(), error = %e, "touch failed");
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Apparent bytes of every file under `dir`; 0 if it is gone.
fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}
