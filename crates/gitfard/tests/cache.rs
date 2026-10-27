use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitfar_proto::{ErrorCode, WarmingState};
use gitfard::cache::RepoCache;
use gitfard::config::Credential;
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

/// A `git` wrapper that logs every clone and fetch start/end to `ops.log`. While `<tmp>/limited`
/// exists (content: `<status> [<retry-after secs>]`) they fail the way git reports an HTTP error
/// under `GIT_TRACE_CURL`; while `<tmp>/missing` exists, the way it reports a 404. `slow` slows them.
fn upstream_git(fx: &Fixture) -> PathBuf {
    let path = fx.tmp.path().join("upstream-git");
    let dir = fx.tmp.path().display();
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
case " $* " in *" fetch "*|*" clone "*)
  case " $* " in *" fetch "*) op=fetch;; *) op=clone;; esac
  echo "+$op" >> '{dir}/ops.log'
  [ -e '{dir}/slow' ] && sleep 0.5
  if [ -e '{dir}/limited' ]; then
    read status after < '{dir}/limited'
    echo "12:00:00.000000 http.c:756          <= Recv header: HTTP/2 $status" >&2
    [ -n "$after" ] && echo "12:00:00.000000 http.c:756          <= Recv header: Retry-After: $after" >&2
    echo "12:00:00.000000 http.c:756          => Send header: Authorization: Basic <redacted>" >&2
    echo "fatal: unable to access 'https://example.com/': The requested URL returned error: $status" >&2
    echo "-$op" >> '{dir}/ops.log'
    exit 128
  fi
  if [ -e '{dir}/missing' ]; then
    echo "remote: Repository not found." >&2
    echo "fatal: repository 'https://example.com/acme/x.git/' not found" >&2
    echo "-$op" >> '{dir}/ops.log'
    exit 128
  fi
  git "$@"; rc=$?
  echo "-$op" >> '{dir}/ops.log'
  exit $rc;;
esac
exec git "$@"
"#
        ),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&path).status().unwrap();
    path
}

impl Fixture {
    fn open_limited(&self, branch_ttl: Duration, upstream: &str) -> Arc<RepoCache> {
        let base = format!("file://{}/up/", self.tmp.path().display());
        let limits: gitfard::config::Upstream = toml::from_str(upstream).unwrap();
        Arc::new(
            RepoCache::open(&self.cache_dir(), 2)
                .unwrap()
                .with_upstream(&base, &upstream_git(self))
                .with_refresh(branch_ttl, WAIT)
                .with_limits(&limits),
        )
    }

    /// `+op` / `-op` lines: each clone or fetch start and end, in order.
    fn ops(&self) -> Vec<String> {
        std::fs::read_to_string(self.tmp.path().join("ops.log"))
            .map_or(vec![], |s| s.lines().map(str::to_owned).collect())
    }

    fn count(&self, op: &str) -> usize {
        let start = format!("+{op}");
        self.ops().iter().filter(|l| **l == start).count()
    }

    fn limit(&self, line: &str) {
        std::fs::write(self.tmp.path().join("limited"), line).unwrap();
    }

    fn unlimit(&self) {
        std::fs::remove_file(self.tmp.path().join("limited")).unwrap();
    }
}

#[tokio::test]
async fn rate_limited_fetch_cools_the_host_for_retry_after_and_answers_the_old_sha() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_limited(Duration::ZERO, "");
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    let old = trunk(&repo);
    std::fs::remove_file(fx.tmp.path().join("ops.log")).unwrap();
    fx.limit("429 2");

    cache.ensure(&url, None, WAIT).await.unwrap();

    assert_eq!(fx.count("fetch"), 1);
    let cooling = cache.cooldowns();
    assert_eq!(cooling.len(), 1);
    assert_eq!(cooling[0].0, "example.com");
    assert!(
        cooling[0].1 > Duration::from_millis(1500) && cooling[0].1 <= Duration::from_secs(2),
        "{cooling:?}"
    );

    // While cooling, due fetches are skipped and requests answer from the old SHA.
    fx.unlimit();
    let new = fx.commit("x");
    assert_eq!(cache.ensure(&url, None, WAIT).await.unwrap(), repo);
    assert_eq!(fx.count("fetch"), 1);
    assert_eq!(trunk(&repo), old);

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(cache.cooldowns().is_empty());
    cache.ensure(&url, None, WAIT).await.unwrap();
    assert_eq!(fx.count("fetch"), 2);
    assert_eq!(trunk(&repo), new);
}

#[tokio::test]
async fn rate_limit_without_retry_after_backs_off_from_the_initial_cooldown() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_limited(Duration::ZERO, "cooldown_initial = \"10s\"");
    cache.ensure(&url, None, WAIT).await.unwrap();
    fx.limit("503");

    cache.ensure(&url, None, WAIT).await.unwrap();

    let cooling = cache.cooldowns();
    assert!(
        cooling[0].1 > Duration::from_secs(7) && cooling[0].1 <= Duration::from_secs(10),
        "{cooling:?}"
    );
}

#[tokio::test]
async fn rate_limited_clone_stays_queued_and_reports_rate_limited() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_limited(Duration::from_secs(3600), "");
    fx.limit("429 1");

    let err = cache
        .ensure(&url, None, Duration::from_millis(500))
        .await
        .unwrap_err();
    assert_eq!(err.error.code, ErrorCode::RepoWarming);
    assert_eq!(err.error.progress.unwrap().state, WarmingState::RateLimited);
    let queue = cache.clone_queue();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].0, url);
    assert_eq!(queue[0].1.state, WarmingState::RateLimited);

    // Not negatively cached: once the cooldown ends the queued clone runs again.
    fx.unlimit();
    cache.ensure(&url, None, WAIT).await.unwrap();
    assert_eq!(fx.count("clone"), 2);
    assert!(cache.clone_queue().is_empty());
}

#[tokio::test]
async fn missing_repo_is_not_found_and_upstream_is_not_asked_again_within_the_negative_cache_ttl() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_limited(Duration::from_secs(3600), "negative_cache_ttl = \"1s\"");
    fx.touch("missing");

    for _ in 0..3 {
        let err = cache.ensure(&url, None, WAIT).await.unwrap_err();
        assert_eq!(err.error.code, ErrorCode::NotFound);
    }
    assert_eq!(fx.count("clone"), 1);

    std::fs::remove_file(fx.tmp.path().join("missing")).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    cache.ensure(&url, None, WAIT).await.unwrap();
    assert_eq!(fx.count("clone"), 2);
}

#[tokio::test]
async fn fifth_concurrent_fetch_to_one_host_waits() {
    let fx = Fixture::new();
    let urls: Vec<_> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|n| fx.upstream(n))
        .collect();
    let cache = fx.open_limited(Duration::ZERO, "");
    for url in &urls {
        cache.ensure(url, None, WAIT).await.unwrap();
    }
    std::fs::remove_file(fx.tmp.path().join("ops.log")).unwrap();
    fx.touch("slow");

    let tasks: Vec<_> = urls
        .iter()
        .map(|url| {
            let (cache, url) = (cache.clone(), url.clone());
            tokio::spawn(async move { cache.ensure(&url, None, WAIT).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }

    let (mut running, mut peak) = (0, 0);
    for op in fx.ops() {
        running += if op.starts_with('+') { 1 } else { -1 };
        peak = peak.max(running);
    }
    assert_eq!(fx.count("fetch"), 5);
    assert_eq!(peak, 4, "{:?}", fx.ops());
}
