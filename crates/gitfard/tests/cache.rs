use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitfar_proto::{ErrorCode, WarmingState};
use gitfard::cache::RepoCache;
use gitfard::config::{ConfigHandle, Credential};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}");
    String::from_utf8(o.stdout).unwrap()
}

/// Upstreams live at `<tmp>/up/example.com/acme/<name>.git`, served as `file://<tmp>/up/`.
struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            tmp: TempDir::new().unwrap(),
        }
    }

    /// An upstream whose default branch is `trunk`, with a tag and a platform-private ref.
    fn upstream(&self, name: &str) -> String {
        let work = self.tmp.path().join("work").join(name);
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "trunk"]);
        std::fs::write(work.join("a.txt"), "hello\n").unwrap();
        git(&work, &["add", "-A"]);
        git(
            &work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );
        git(&work, &["tag", "v1"]);
        let dst = self
            .tmp
            .path()
            .join(format!("up/example.com/acme/{name}.git"));
        git(
            &work,
            &["clone", "-q", "--bare", ".", dst.to_str().unwrap()],
        );
        git(&dst, &["update-ref", "refs/pull/1/head", "HEAD"]);
        format!("example.com/acme/{name}")
    }

    fn cache_dir(&self) -> PathBuf {
        self.tmp.path().join("cache")
    }

    /// A `git` wrapper that logs and slows down every clone, so tests can count and overlap them.
    fn slow_git(&self) -> PathBuf {
        let path = self.tmp.path().join("slow-git");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = clone ]; then echo x >> '{}'; sleep 1; fi\nexec git \"$@\"\n",
                self.clone_log().display()
            ),
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&path).status().unwrap();
        path
    }

    fn clone_log(&self) -> PathBuf {
        self.tmp.path().join("clones.log")
    }

    fn clones(&self) -> usize {
        std::fs::read_to_string(self.clone_log()).map_or(0, |s| s.lines().count())
    }

    fn open(&self, concurrency: usize, git: &Path) -> Arc<RepoCache> {
        self.open_refreshing(
            concurrency,
            git,
            Duration::from_secs(60),
            Duration::from_secs(5),
        )
    }

    fn open_refreshing(
        &self,
        concurrency: usize,
        git: &Path,
        branch_ttl: Duration,
        fetch_timeout: Duration,
    ) -> Arc<RepoCache> {
        let base = format!("file://{}/up/", self.tmp.path().display());
        Arc::new(
            RepoCache::open(&self.cache_dir(), concurrency)
                .unwrap()
                .with_upstream(&base, git)
                .with_refresh(branch_ttl, fetch_timeout),
        )
    }

    /// Pushes a new commit to upstream `name`'s `trunk`; returns its SHA.
    fn commit(&self, name: &str) -> String {
        let work = self.tmp.path().join("work").join(name);
        let n = git(&work, &["rev-list", "--count", "HEAD"]);
        std::fs::write(work.join("a.txt"), n).unwrap();
        git(
            &work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qam",
                "more",
            ],
        );
        let dst = self
            .tmp
            .path()
            .join(format!("up/example.com/acme/{name}.git"));
        git(&work, &["push", "-q", dst.to_str().unwrap(), "trunk"]);
        git(&work, &["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// A `git` wrapper that logs every fetch. Touching `hang` makes fetches hang; touching
    /// `revoked` makes them fail the way a rejected credential does.
    fn fetch_git(&self) -> PathBuf {
        let path = self.tmp.path().join("fetch-git");
        let dir = self.tmp.path().display();
        std::fs::write(
            &path,
            format!(
                r#"#!/bin/sh
case " $* " in *" fetch "*)
  echo x >> '{dir}/fetches.log'
  [ -e '{dir}/hang' ] && sleep 30
  [ -e '{dir}/revoked' ] && {{ echo "fatal: Authentication failed for 'https://example.com/'" >&2; exit 128; }}
  sleep 0.3;;
esac
exec git "$@"
"#
            ),
        )
        .unwrap();
        Command::new("chmod").arg("+x").arg(&path).status().unwrap();
        path
    }

    fn fetches(&self) -> usize {
        std::fs::read_to_string(self.tmp.path().join("fetches.log"))
            .map_or(0, |s| s.lines().count())
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.tmp.path().join(name), "").unwrap();
    }
}

fn trunk(repo: &Path) -> String {
    git(repo, &["rev-parse", "refs/heads/trunk"])
        .trim()
        .to_owned()
}

fn packs(repo: &Path) -> usize {
    std::fs::read_dir(repo.join("objects/pack"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "pack")
        })
        .count()
}

const WAIT: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_millis(100);

#[tokio::test]
async fn first_request_clones_a_full_bare_mirror_of_heads_and_tags() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open(2, Path::new("git"));

    let repo = cache.ensure(&url, None, WAIT).await.unwrap();

    assert_eq!(repo, fx.cache_dir().join("repos/example.com/acme/x.git"));
    assert_eq!(git(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/trunk\n");
    assert_eq!(
        git(&repo, &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/trunk\nrefs/tags/v1\n"
    );
    assert!(repo.join("objects/info/commit-graph").exists());
    assert!(repo.join("gitfar-access").exists(), "touch file");
}

#[tokio::test]
async fn concurrent_requests_share_one_clone() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open(2, &fx.slow_git());

    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let (cache, url) = (cache.clone(), url.clone());
            tokio::spawn(async move { cache.ensure(&url, None, WAIT).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(fx.clones(), 1);

    cache.ensure(&url, None, WAIT).await.unwrap();
    assert_eq!(fx.clones(), 1, "cached repo is not cloned again");
}

#[tokio::test]
async fn third_concurrent_clone_queues_and_reports_its_position() {
    let fx = Fixture::new();
    let urls: Vec<_> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| fx.upstream(n))
        .collect();
    let cache = fx.open(2, &fx.slow_git());

    let mut progress = Vec::new();
    for url in &urls {
        let err = cache.ensure(url, None, SHORT).await.unwrap_err();
        assert_eq!(err.error.code, ErrorCode::RepoWarming);
        let p = err.error.progress.unwrap();
        progress.push((p.state, p.queue_position));
    }
    assert_eq!(
        progress,
        [
            (WarmingState::Cloning, None),
            (WarmingState::Cloning, None),
            (WarmingState::Queued, Some(1)),
            (WarmingState::Queued, Some(2)),
        ]
    );

    for url in &urls {
        cache.ensure(url, None, WAIT).await.unwrap();
    }
    assert_eq!(fx.clones(), 4);
}

#[tokio::test]
async fn failed_clone_is_an_error_and_the_next_request_retries() {
    let fx = Fixture::new();
    let cache = fx.open(2, Path::new("git"));
    let url = "example.com/acme/later";

    let err = cache.ensure(url, None, WAIT).await.unwrap_err();
    assert_eq!(err.error.code, ErrorCode::UpstreamUnavailable);
    assert!(!fx
        .cache_dir()
        .join("repos/example.com/acme/later.git")
        .exists());

    fx.upstream("later");
    cache.ensure(url, None, WAIT).await.unwrap();
}

#[tokio::test]
async fn restart_discards_half_finished_clones() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    // What a killed process leaves behind: a partial clone in the temp area, nothing under repos/.
    std::fs::create_dir_all(fx.cache_dir().join("tmp/7/objects")).unwrap();

    let cache = fx.open(2, Path::new("git"));

    assert_eq!(
        std::fs::read_dir(fx.cache_dir().join("tmp"))
            .unwrap()
            .count(),
        0
    );
    cache.ensure(&url, None, WAIT).await.unwrap();
    assert_eq!(
        std::fs::read_dir(fx.cache_dir().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn a_second_process_cannot_open_the_same_cache_dir() {
    let fx = Fixture::new();
    let _first = RepoCache::open(&fx.cache_dir(), 2).unwrap();
    assert!(RepoCache::open(&fx.cache_dir(), 2).is_err());
}

#[tokio::test]
async fn clone_with_a_credential_leaves_no_secret_in_repo_config() {
    let fx = Fixture::new();
    let url = fx.upstream("private");
    let cred: Credential =
        toml::from_str("prefix = \"example.com/acme\"\ntoken = \"s3cr3t-token\"").unwrap();
    // Records the env-injected git config the clone ran with.
    let spy = fx.tmp.path().join("spy-git");
    let seen = fx.tmp.path().join("seen");
    std::fs::write(
        &spy,
        format!(
            "#!/bin/sh\n[ \"$1\" = clone ] && echo \"$GIT_CONFIG_KEY_0=$GIT_CONFIG_VALUE_0\" > '{}'\nexec git \"$@\"\n",
            seen.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&spy).status().unwrap();
    let cache = fx.open(2, &spy);

    let repo = cache.ensure(&url, Some(&cred), WAIT).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        format!(
            "http.file://{}/up/example.com/acme/.extraHeader=Authorization: Basic eC1hY2Nlc3MtdG9rZW46czNjcjN0LXRva2Vu\n",
            fx.tmp.path().display()
        )
    );
    let config = std::fs::read_to_string(repo.join("config")).unwrap();
    assert!(!config.contains("s3cr3t"), "{config}");
    assert!(!config.contains("Basic"), "{config}");
    assert!(
        !config.to_ascii_lowercase().contains("extraheader"),
        "{config}"
    );
}

#[tokio::test]
async fn expired_branch_ttl_fetches_once_for_concurrent_requests() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let ttl = Duration::from_millis(300);
    let cache = fx.open_refreshing(2, &fx.fetch_git(), ttl, WAIT);
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    let new = fx.commit("x");
    tokio::time::sleep(ttl).await;

    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let (cache, url) = (cache.clone(), url.clone());
            tokio::spawn(async move { cache.ensure(&url, None, WAIT).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }

    assert_eq!(fx.fetches(), 1);
    assert_eq!(trunk(&repo), new);
}

#[tokio::test]
async fn within_branch_ttl_requests_do_not_fetch() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_refreshing(2, &fx.fetch_git(), Duration::from_secs(3600), WAIT);
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    let old = trunk(&repo);
    fx.commit("x");

    cache.ensure(&url, None, WAIT).await.unwrap();

    assert_eq!(fx.fetches(), 0);
    assert_eq!(trunk(&repo), old);
}

#[tokio::test]
async fn unreachable_upstream_answers_from_the_old_sha_after_the_fetch_timeout() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let timeout = Duration::from_millis(800);
    let cache = fx.open_refreshing(2, &fx.fetch_git(), Duration::ZERO, timeout);
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    let old = trunk(&repo);
    fx.commit("x");
    fx.touch("hang");
    let before = fx.fetches();

    let start = Instant::now();
    cache.ensure(&url, None, WAIT).await.unwrap();
    let took = start.elapsed();

    assert!(took >= timeout && took < timeout * 3, "{took:?}");
    assert_eq!(trunk(&repo), old);
    // The fetch carries on in the background; requests meanwhile join it without waiting again.
    let start = Instant::now();
    cache.ensure(&url, None, WAIT).await.unwrap();
    assert!(start.elapsed() < timeout / 2);
    assert_eq!(fx.fetches() - before, 1);
}

#[tokio::test]
async fn revoked_credential_makes_the_repo_unavailable_until_a_fetch_succeeds() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_refreshing(2, &fx.fetch_git(), Duration::ZERO, WAIT);
    cache.ensure(&url, None, WAIT).await.unwrap();
    fx.touch("revoked");

    for _ in 0..2 {
        let err = cache.ensure(&url, None, WAIT).await.unwrap_err();
        assert_eq!(err.error.code, ErrorCode::UpstreamUnavailable);
    }

    std::fs::remove_file(fx.tmp.path().join("revoked")).unwrap();
    cache.ensure(&url, None, WAIT).await.unwrap();
}

#[tokio::test]
async fn unknown_rev_refetches_unless_fetched_within_a_minute() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let ttl = Duration::from_secs(3600);
    let cache = fx.open_refreshing(2, &fx.fetch_git(), ttl, WAIT);
    cache.ensure(&url, None, WAIT).await.unwrap();
    fx.commit("x");

    cache.refetch_for_unknown_rev(&url, None).await;
    assert_eq!(fx.fetches(), 0, "just cloned");

    // After a restart the last fetch time is unknown, so a fetch is due.
    drop(cache);
    let cache = fx.open_refreshing(2, &fx.fetch_git(), ttl, WAIT);
    let new = fx.commit("x");
    cache.refetch_for_unknown_rev(&url, None).await;
    cache.refetch_for_unknown_rev(&url, None).await;
    assert_eq!(fx.fetches(), 1);
    assert_eq!(trunk(&cache.path(&url)), new);
}

#[tokio::test]
async fn fetch_repacks_when_there_are_more_than_16_packs() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_refreshing(2, Path::new("git"), Duration::ZERO, WAIT);
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    for i in 0..17 {
        std::fs::write(repo.join("blob"), format!("blob {i}\n")).unwrap();
        let sha = git(&repo, &["hash-object", "-w", "blob"]);
        // Plain repack packs only reachable loose objects.
        git(
            &repo,
            &["update-ref", &format!("refs/tags/b{i}"), sha.trim()],
        );
        git(&repo, &["repack", "-q"]);
    }
    assert!(packs(&repo) > 16);

    cache.ensure(&url, None, WAIT).await.unwrap();

    assert!(packs(&repo) <= 2, "{} packs", packs(&repo));
}

// ---- 淘汰, cache_full, 维护 (spec §4.5) ----

/// Writes `<tmp>/gitfard.toml` with the given top-level and `[cache]` keys; `allowlist`
/// defaults to `example.com/acme`, `disk_budget` to 1 TB and `disk_min_free` to 0.
fn write_config(fx: &Fixture, top: &str, cache: &str) -> PathBuf {
    let path = fx.tmp.path().join("gitfard.toml");
    let mut top = top.to_owned();
    if !top.contains("allowlist") {
        top += "\nallowlist = [\"example.com/acme\"]";
    }
    let mut cache = cache.to_owned();
    if !cache.contains("disk_budget") {
        cache += "\ndisk_budget = \"1TB\"";
    }
    // The 10 GB default would make tests depend on the box's free disk.
    if !cache.contains("disk_min_free") {
        cache += "\ndisk_min_free = \"0B\"";
    }
    std::fs::write(
        &path,
        format!("{top}\n[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/unused\"\n[cache]\n{cache}\n"),
    )
    .unwrap();
    path
}

fn open_with(fx: &Fixture, git: &Path) -> (Arc<RepoCache>, Arc<ConfigHandle>) {
    let cfg = Arc::new(ConfigHandle::load(write_config(fx, "", "")).unwrap());
    let base = format!("file://{}/up/", fx.tmp.path().display());
    let cache = RepoCache::open(&fx.cache_dir(), 2)
        .unwrap()
        .with_upstream(&base, git)
        .with_config(cfg.clone());
    (Arc::new(cache), cfg)
}

/// Apparent bytes of every file under `dir`.
fn size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let m = e.metadata().unwrap();
            if m.is_dir() {
                size(&e.path())
            } else {
                m.len()
            }
        })
        .sum()
}

const OUTSIDE_WINDOW: u16 = 12 * 60;
const INSIDE_WINDOW: u16 = 4 * 60;
// No real disk has 1000 TB free, so this simulates a full disk.
const DISK_FULL: &str = "disk_min_free = \"1000TB\"";

#[tokio::test]
async fn over_budget_evicts_the_least_recently_used_repo_except_warm_list() {
    let fx = Fixture::new();
    let [a, b, c] = ["a", "b", "c"].map(|n| fx.upstream(n));
    let (cache, cfg) = open_with(&fx, &fx.slow_git());
    for url in [&a, &b, &c, &a] {
        cache.ensure(url, None, WAIT).await.unwrap();
    }
    // LRU order is now b, c, a; b is on the 预热列表, so c goes first.
    let total: u64 = [&a, &b, &c].iter().map(|u| size(&cache.path(u))).sum();
    write_config(
        &fx,
        &format!("warm_list = [\"{b}\"]"),
        &format!("disk_budget = \"{}B\"", total - 1),
    );
    cfg.reload().unwrap();

    cache.tick(OUTSIDE_WINDOW).await;

    assert!(cache.path(&a).exists());
    assert!(cache.path(&b).exists());
    assert!(!cache.path(&c).exists());
    cache.ensure(&c, None, WAIT).await.unwrap();
    assert_eq!(fx.clones(), 4, "an evicted repo is 预热ed again");
}

#[tokio::test]
async fn low_free_disk_refuses_new_clones_but_cached_repos_still_serve() {
    let fx = Fixture::new();
    let [a, b] = ["a", "b"].map(|n| fx.upstream(n));
    let (cache, cfg) = open_with(&fx, Path::new("git"));
    cache.ensure(&a, None, WAIT).await.unwrap();

    write_config(&fx, "", DISK_FULL);
    cfg.reload().unwrap();

    let err = cache.ensure(&b, None, WAIT).await.unwrap_err();
    assert_eq!(err.error.code, ErrorCode::CacheFull);
    assert_eq!(cache.ensure(&a, None, WAIT).await.unwrap(), cache.path(&a));
}

#[tokio::test]
async fn disk_running_low_aborts_the_clone_in_progress_and_cleans_up() {
    let fx = Fixture::new();
    let a = fx.upstream("a");
    let (cache, cfg) = open_with(&fx, &fx.slow_git());

    let pending = {
        let (cache, a) = (cache.clone(), a.clone());
        tokio::spawn(async move { cache.ensure(&a, None, WAIT).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    write_config(&fx, "", DISK_FULL);
    cfg.reload().unwrap();

    let err = pending.await.unwrap().unwrap_err();
    assert_eq!(err.error.code, ErrorCode::CacheFull);
    assert!(!cache.path(&a).exists());
    assert_eq!(
        std::fs::read_dir(fx.cache_dir().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn full_gc_and_allowlist_cleanup_run_only_inside_the_maintenance_window() {
    let fx = Fixture::new();
    let [a, gone] = ["a", "gone"].map(|n| fx.upstream(n));
    // Logs every `git gc` run.
    let spy = fx.tmp.path().join("gc-git");
    let gcs = fx.tmp.path().join("gcs.log");
    std::fs::write(
        &spy,
        format!(
            "#!/bin/sh\nfor a; do [ \"$a\" = gc ] && echo gc >> '{}'; done\nexec git \"$@\"\n",
            gcs.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&spy).status().unwrap();
    let (cache, cfg) = open_with(&fx, &spy);
    cache.ensure(&a, None, WAIT).await.unwrap();
    cache.ensure(&gone, None, WAIT).await.unwrap();
    write_config(&fx, &format!("allowlist = [\"{a}\"]"), "");
    cfg.reload().unwrap();
    let gc_runs = || std::fs::read_to_string(&gcs).map_or(0, |s| s.lines().count());

    cache.tick(OUTSIDE_WINDOW).await;
    assert_eq!(gc_runs(), 0, "no full gc outside the window");
    assert!(cache.path(&gone).exists());

    cache.tick(INSIDE_WINDOW).await;
    assert_eq!(gc_runs(), 1);
    assert!(!cache.path(&gone).exists(), "no longer allowlisted");
    cache.ensure(&a, None, WAIT).await.unwrap();

    cache.tick(INSIDE_WINDOW).await;
    assert_eq!(gc_runs(), 1, "gc is daily, not every tick");
}
