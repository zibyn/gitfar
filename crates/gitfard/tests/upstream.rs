use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitfard::config::ConfigHandle;
use gitfard::git::GitFailure;
use gitfard::upstream::UpstreamLimits;
use tempfile::TempDir;

const H: &str = "example.com";
const LIMITED: Result<(), GitFailure> = Err(GitFailure::RateLimited { retry_after: None });

fn after(secs: u64) -> Result<(), GitFailure> {
    Err(GitFailure::RateLimited {
        retry_after: Some(Duration::from_secs(secs)),
    })
}

fn write(path: &Path, upstream: &str) {
    std::fs::write(
        path,
        format!("[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\n[cache]\ndisk_budget = \"1GB\"\n[upstream]\n{upstream}\n"),
    )
    .unwrap();
}

fn limits(upstream: &str) -> (TempDir, PathBuf, Arc<ConfigHandle>, UpstreamLimits) {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("gitfard.toml");
    write(&path, upstream);
    let cfg = Arc::new(ConfigHandle::load(&path).unwrap());
    let limits = UpstreamLimits::new(cfg.clone());
    (tmp, path, cfg, limits)
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

#[test]
fn first_rate_limit_cools_for_the_initial_cooldown_less_up_to_20_percent_jitter() {
    let (_t, _p, _c, l) = limits("cooldown_initial = \"10s\"");
    let now = Instant::now();
    for i in 0..50 {
        let host = format!("h{i}");
        let left = l.record(&host, LIMITED, now).unwrap() - now;
        assert!((8.0..=10.0).contains(&secs(left)), "{left:?}");
    }
}

#[test]
fn consecutive_rate_limits_double_the_backoff_up_to_cooldown_max() {
    let (_t, _p, _c, l) = limits("cooldown_initial = \"10s\"\ncooldown_max = \"25s\"");
    let now = Instant::now();
    let ends: Vec<_> = (0..4)
        .map(|_| secs(l.record(H, LIMITED, now).unwrap() - now))
        .collect();
    assert!((16.0..=20.0).contains(&ends[1]), "{ends:?}");
    assert!((20.0..=25.0).contains(&ends[2]), "{ends:?}");
    assert!((20.0..=25.0).contains(&ends[3]), "{ends:?}");
}

#[test]
fn retry_after_wins_over_backoff() {
    let (_t, _p, _c, l) = limits("cooldown_initial = \"10s\"");
    let now = Instant::now();
    assert_eq!(
        l.record(H, after(100), now),
        Some(now + Duration::from_secs(100))
    );
}

#[test]
fn a_cooldown_is_extended_but_never_shortened() {
    let (_t, _p, _c, l) = limits("");
    let now = Instant::now();
    l.record(H, after(100), now);
    let end = now + Duration::from_secs(100);
    assert_eq!(l.record(H, after(5), now), Some(end));
    assert_eq!(l.cool_until(H, now), Some(end));
    assert_eq!(
        l.record(H, after(200), now),
        Some(now + Duration::from_secs(200))
    );
}

#[test]
fn success_resets_the_backoff() {
    let (_t, _p, _c, l) = limits("cooldown_initial = \"10s\"");
    let now = Instant::now();
    for _ in 0..3 {
        l.record(H, LIMITED, now);
    }
    assert_eq!(l.record(H, Ok(()), now), None);

    let later = now + Duration::from_secs(3600);
    let left = l.record(H, LIMITED, later).unwrap() - later;
    assert!((8.0..=10.0).contains(&secs(left)), "{left:?}");
}

#[test]
fn other_failures_do_not_cool() {
    let (_t, _p, _c, l) = limits("");
    let now = Instant::now();
    for f in [
        GitFailure::NotFound,
        GitFailure::AuthFailed,
        GitFailure::Other,
    ] {
        assert_eq!(l.record(H, Err(f), now), None);
    }
    assert!(!l.cooling(H, now));
}

#[test]
fn cooling_ends_at_its_end_and_leaves_the_snapshot() {
    let (_t, _p, _c, l) = limits("");
    let now = Instant::now();
    let end = l.record(H, after(30), now).unwrap();
    l.record("other.example", after(10), now);

    assert!(l.cooling(H, now));
    assert_eq!(
        l.snapshot(now),
        [
            (H.to_owned(), Duration::from_secs(30)),
            ("other.example".to_owned(), Duration::from_secs(10)),
        ]
    );
    assert!(!l.cooling(H, end));
    assert_eq!(l.cool_until(H, end), None);
    assert!(l.snapshot(end).is_empty());
}

/// Whether a slot is free right now, without waiting for one.
async fn free(l: &UpstreamLimits, host: &str) -> Option<tokio::sync::OwnedSemaphorePermit> {
    tokio::time::timeout(Duration::ZERO, l.slot(host))
        .await
        .ok()
}

#[tokio::test]
async fn host_concurrency_bounds_slots_per_host_and_a_reload_resizes_them() {
    let (_t, path, cfg, l) = limits("host_concurrency = 2");
    let held: Vec<_> = [free(&l, H).await, free(&l, H).await]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(held.len(), 2);
    assert!(free(&l, H).await.is_none());
    assert!(
        free(&l, "other.example").await.is_some(),
        "slots are per host"
    );

    write(&path, "host_concurrency = 3");
    cfg.reload().unwrap();

    let mut resized = Vec::new();
    while let Some(p) = free(&l, H).await {
        resized.push(p);
        assert!(resized.len() <= 3);
    }
    assert_eq!(resized.len(), 3);
}
