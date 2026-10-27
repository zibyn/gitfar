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
        let cache = format!(
            "clone_concurrency = {concurrency}\nbranch_ttl = \"{}ms\"\nfetch_timeout = \"{}ms\"",
            branch_ttl.as_millis(),
            fetch_timeout.as_millis()
        );
        open_config(self, git, write_config_with(self, "", &cache, "")).0
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
    let cfg = Arc::new(ConfigHandle::load(write_config(&fx, "", "")).unwrap());
    let _first = RepoCache::open(cfg.clone()).unwrap();
    assert!(RepoCache::open(cfg).is_err());
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
async fn fetch_makes_head_follow_a_renamed_upstream_default_branch() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let ttl = Duration::from_millis(100);
    let cache = fx.open_refreshing(2, Path::new("git"), ttl, WAIT);
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    let up = fx.tmp.path().join("up/example.com/acme/x.git");
    git(&up, &["branch", "-m", "trunk", "main"]);
    tokio::time::sleep(ttl).await;

    cache.ensure(&url, None, WAIT).await.unwrap();

    assert_eq!(git(&repo, &["symbolic-ref", "HEAD"]), "refs/heads/main\n");
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
        let cache = format!(
            "branch_ttl = \"{}ms\"\nfetch_timeout = \"{}ms\"",
            branch_ttl.as_millis(),
            WAIT.as_millis()
        );
        let path = write_config_with(self, "", &cache, upstream);
        open_config(self, &upstream_git(self), path).0
    }

    /// `+op` / `-op` lines: each clone or fetch start and end, in order.
    fn ops(&self) -> Vec<String> {
        std::fs::read_to_string(self.tmp.path().join("ops.log"))
            .map_or(vec![], |s| s.lines().map(str::to_owned).collect())
    }

    /// Most clones and fetches ever running at once, from `ops.log`.
    fn peak(&self) -> i32 {
        let (mut running, mut peak) = (0, 0);
        for op in self.ops() {
            running += if op.starts_with('+') { 1 } else { -1 };
            peak = peak.max(running);
        }
        peak
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
async fn rate_limited_warming_suggests_retrying_when_the_cooldown_ends() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open_limited(Duration::from_secs(3600), "");
    fx.limit("429 30");

    let err = cache.ensure(&url, None, SHORT).await.unwrap_err();

    assert_eq!(err.error.progress.unwrap().state, WarmingState::RateLimited);
    let left = err.retry_after.unwrap();
    assert!(
        left > Duration::from_secs(25) && left <= Duration::from_secs(30),
        "{left:?}"
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

    assert_eq!(fx.count("fetch"), 5);
    assert_eq!(fx.peak(), 4, "{:?}", fx.ops());
}

#[tokio::test]
async fn a_fetch_waiting_for_a_slot_is_skipped_once_the_host_cools() {
    let fx = Fixture::new();
    let [a, b] = ["a", "b"].map(|n| fx.upstream(n));
    let cache = fx.open_limited(
        Duration::ZERO,
        "host_concurrency = 1\ncooldown_initial = \"2s\"",
    );
    for url in [&a, &b] {
        cache.ensure(url, None, WAIT).await.unwrap();
    }
    std::fs::remove_file(fx.tmp.path().join("ops.log")).unwrap();
    fx.touch("slow");
    fx.limit("429");

    let tasks: Vec<_> = [&a, &b]
        .map(|url| {
            let (cache, url) = (cache.clone(), url.clone());
            tokio::spawn(async move { cache.ensure(&url, None, WAIT).await })
        })
        .into_iter()
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }

    assert_eq!(fx.count("fetch"), 1, "{:?}", fx.ops());
    // One strike only: a second 429 would have doubled the backoff past 2 s.
    let cooling = cache.cooldowns();
    assert!(cooling[0].1 <= Duration::from_secs(2), "{cooling:?}");

    fx.unlimit();
    tokio::time::sleep(cooling[0].1).await;
    cache.ensure(&b, None, WAIT).await.unwrap();
    assert_eq!(fx.count("fetch"), 2);
}

#[tokio::test]
async fn a_clone_waiting_for_a_slot_stays_queued_once_the_host_cools() {
    let fx = Fixture::new();
    let [a, b] = ["a", "b"].map(|n| fx.upstream(n));
    let cache = fx.open_limited(
        Duration::from_secs(3600),
        "host_concurrency = 1\ncooldown_initial = \"2s\"",
    );
    fx.touch("slow");
    fx.limit("429");

    for url in [&a, &b] {
        cache.ensure(url, None, SHORT).await.unwrap_err();
    }
    // a's clone takes 0.5 s and gets the 429; b has had its slot well before this.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(fx.count("clone"), 1, "{:?}", fx.ops());
    let cooling = cache.cooldowns();
    assert!(cooling[0].1 <= Duration::from_secs(2), "{cooling:?}");

    fx.unlimit();
    for url in [&a, &b] {
        cache.ensure(url, None, WAIT).await.unwrap();
    }
    // a's 429, then a and b after the cooldown; b's skipped attempt never reached upstream.
    assert_eq!(fx.count("clone"), 3);
}

#[tokio::test]
async fn reloaded_clone_concurrency_applies_to_queued_clones() {
    let fx = Fixture::new();
    let urls: Vec<_> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| fx.upstream(n))
        .collect();
    let path = write_config_with(&fx, "", "clone_concurrency = 1", "");
    let (cache, cfg) = open_config(&fx, &upstream_git(&fx), path);
    fx.touch("slow");
    for url in &urls {
        cache.ensure(url, None, SHORT).await.unwrap_err();
    }

    write_config_with(&fx, "", "clone_concurrency = 2", "");
    cfg.reload().unwrap();
    for url in &urls {
        cache.ensure(url, None, WAIT).await.unwrap();
    }

    assert_eq!(fx.count("clone"), 4);
    assert_eq!(fx.peak(), 2, "{:?}", fx.ops());
}

// ---- 淘汰, cache_full, 维护 (spec §4.5) ----

/// Writes `<tmp>/gitfard.toml` with the given top-level and `[cache]` keys; `allowlist`
/// defaults to `example.com/acme`, `disk_budget` to 1 TB and `disk_min_free` to 0.
fn write_config(fx: &Fixture, top: &str, cache: &str) -> PathBuf {
    write_config_with(fx, top, cache, "")
}

/// [`write_config`] plus `[upstream]` keys.
fn write_config_with(fx: &Fixture, top: &str, cache: &str, upstream: &str) -> PathBuf {
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
        format!(
            "{top}\n[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"{}\"\n[cache]\n{cache}\n[upstream]\n{upstream}\n",
            fx.cache_dir().display()
        ),
    )
    .unwrap();
    path
}

fn open_with(fx: &Fixture, git: &Path) -> (Arc<RepoCache>, Arc<ConfigHandle>) {
    open_config(fx, git, write_config(fx, "", ""))
}

fn open_config(fx: &Fixture, git: &Path, config: PathBuf) -> (Arc<RepoCache>, Arc<ConfigHandle>) {
    let cfg = Arc::new(ConfigHandle::load(config).unwrap());
    let base = format!("file://{}/up/", fx.tmp.path().display());
    let cache = RepoCache::open(cfg.clone())
        .unwrap()
        .with_upstream(&base, git);
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

#[tokio::test]
async fn a_repo_evicted_before_the_request_gets_its_read_lock_is_warmed_again() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let cache = fx.open(2, &fx.slow_git());
    cache.ensure(&url, None, WAIT).await.unwrap();

    // 淘汰 holds the write lock while the request is between `ensure` and its read lock.
    let evicting = cache.lock(&url).write_owned().await;
    let request = {
        let (cache, url) = (cache.clone(), url.clone());
        tokio::spawn(async move { cache.ensure_locked(&url, None, WAIT).await })
    };
    tokio::time::sleep(SHORT).await;
    std::fs::remove_dir_all(cache.path(&url)).unwrap();
    drop(evicting);

    let (path, _read) = request.await.unwrap().unwrap();
    assert!(path.join("HEAD").exists());
    assert_eq!(fx.clones(), 2);
}

#[tokio::test]
async fn removing_a_repo_keeps_one_lock_for_its_url() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let (cache, cfg) = open_with(&fx, Path::new("git"));
    cache.ensure(&url, None, WAIT).await.unwrap();
    let held = cache.lock(&url);
    write_config(&fx, "allowlist = [\"example.com/other\"]", "");
    cfg.reload().unwrap();

    cache.tick(INSIDE_WINDOW).await;

    assert!(!cache.path(&url).exists());
    assert!(Arc::ptr_eq(&held, &cache.lock(&url)));
}

// ---- SIGHUP hot reload (spec §4.1) ----

#[tokio::test]
async fn reloaded_branch_ttl_applies_to_cached_repos() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let (cache, cfg) = open_with(&fx, &fx.fetch_git());
    let repo = cache.ensure(&url, None, WAIT).await.unwrap();
    let new = fx.commit("x");
    cache.ensure(&url, None, WAIT).await.unwrap();
    assert_eq!(fx.fetches(), 0, "within the default 60 s TTL");

    write_config(&fx, "", "branch_ttl = \"0s\"");
    cfg.reload().unwrap();
    cache.ensure(&url, None, WAIT).await.unwrap();

    assert_eq!(fx.fetches(), 1);
    assert_eq!(trunk(&repo), new);
}

#[tokio::test]
async fn warm_list_entries_added_by_reload_are_warmed() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let (cache, cfg) = open_with(&fx, Path::new("git"));
    write_config(&fx, &format!("warm_list = [\"{url}\"]"), "");
    cfg.reload().unwrap();

    cache.refresh_warm_list();

    let deadline = Instant::now() + WAIT;
    while !cache.path(&url).exists() {
        assert!(Instant::now() < deadline, "warm_list repo never cloned");
        tokio::time::sleep(SHORT).await;
    }
}

#[tokio::test]
async fn maintenance_removes_a_repo_denied_after_reload() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let (cache, cfg) = open_with(&fx, Path::new("git"));
    cache.ensure(&url, None, WAIT).await.unwrap();
    write_config(&fx, &format!("denylist = [\"{url}\"]"), "");
    cfg.reload().unwrap();

    cache.tick(INSIDE_WINDOW).await;

    assert!(!cache.path(&url).exists());
}

#[tokio::test]
async fn denied_warm_list_entries_are_not_warmed() {
    let fx = Fixture::new();
    let url = fx.upstream("x");
    let (cache, cfg) = open_with(&fx, Path::new("git"));
    write_config(
        &fx,
        &format!("warm_list = [\"{url}\"]\ndenylist = [\"{url}\"]"),
        "",
    );
    cfg.reload().unwrap();

    cache.refresh_warm_list();
    tokio::time::sleep(SHORT).await;

    assert!(!cache.path(&url).exists());
}
