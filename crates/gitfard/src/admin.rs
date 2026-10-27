//! 管理监听地址 (spec §6.3): `/metrics`, `/status`, `/healthz`, `/readyz`. No auth, read-only.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

use crate::cache::RepoCache;
use crate::config::ConfigHandle;

pub(crate) type Labels = Vec<(&'static str, String)>;

/// Process-wide metrics; sampled gauges (queue, disk, …) are refreshed on each scrape.
pub(crate) struct Metrics {
    registry: Registry,
    pub requests: Family<Labels, Counter>,
    pub duration: Family<Labels, Histogram, fn() -> Histogram>,
    pub truncations: Family<Labels, Counter>,
    pub fetches: Family<Labels, Counter>,
    pub clones: Family<Labels, Counter>,
    pub warm_fetch_failures: Family<Labels, Gauge>,
    pub evictions: Counter,
    pub blob_hits: Counter,
    pub blob_misses: Counter,
    pub scan_busy: Gauge,
    clone_queue: Gauge,
    clones_running: Gauge,
    host_cooling: Family<Labels, Gauge>,
    host_cooldown: Family<Labels, Gauge<f64, AtomicU64>>,
    /// Hosts ever seen cooling, so they report 0 once the cooldown ends instead of vanishing.
    cooled_hosts: Mutex<BTreeSet<String>>,
    disk_used: Gauge,
    disk_budget: Gauge,
    disk_free: Gauge,
    disk_min_free: Gauge,
    cache_full: Gauge,
    warm_over_budget: Gauge,
    blob_bytes: Gauge,
    blob_capacity: Gauge,
    scan_threads: Gauge,
    git_procs: Gauge,
    git_limit: Gauge,
}

pub(crate) static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let mut r = Registry::with_prefix("gitfar");
    let m = Metrics {
        registry: Registry::default(),
        requests: Family::default(),
        duration: Family::new_with_constructor(|| {
            Histogram::new([0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0])
        }),
        truncations: Family::default(),
        fetches: Family::default(),
        clones: Family::default(),
        warm_fetch_failures: Family::default(),
        evictions: Counter::default(),
        blob_hits: Counter::default(),
        blob_misses: Counter::default(),
        scan_busy: Gauge::default(),
        clone_queue: Gauge::default(),
        clones_running: Gauge::default(),
        host_cooling: Family::default(),
        host_cooldown: Family::default(),
        cooled_hosts: Mutex::default(),
        disk_used: Gauge::default(),
        disk_budget: Gauge::default(),
        disk_free: Gauge::default(),
        disk_min_free: Gauge::default(),
        cache_full: Gauge::default(),
        warm_over_budget: Gauge::default(),
        blob_bytes: Gauge::default(),
        blob_capacity: Gauge::default(),
        scan_threads: Gauge::default(),
        git_procs: Gauge::default(),
        git_limit: Gauge::default(),
    };
    macro_rules! reg {
        ($($name:literal, $help:literal, $field:ident;)*) => {
            $(r.register($name, $help, m.$field.clone());)*
        };
    }
    reg! {
        "requests", "Requests by primitive and code (ok or the 领域错误 code)", requests;
        "request_duration_seconds", "Request latency by primitive and code", duration;
        "truncations", "Truncated responses by reason (limit, bytes, deadline)", truncations;
        "clone_queue_length", "Clones waiting in the queue", clone_queue;
        "clones_running", "Clones in progress", clones_running;
        "host_cooling", "1 while the host is in 冷却", host_cooling;
        "host_cooldown_remaining_seconds", "Time left in the host's 冷却", host_cooldown;
        "fetches", "Finished fetches by result (ok, error)", fetches;
        "clones", "Finished clone attempts by result (ok, error)", clones;
        "warm_repo_consecutive_fetch_failures", "Consecutive failed fetches of a 预热列表 repo", warm_fetch_failures;
        "disk_used_bytes", "Bytes used by cached repos and clones in progress", disk_used;
        "disk_budget_bytes", "disk_budget", disk_budget;
        "disk_free_bytes", "Free bytes on the cache dir's filesystem", disk_free;
        "disk_min_free_bytes", "disk_min_free (the cache_full floor)", disk_min_free;
        "cache_full", "1 while free disk is below disk_min_free", cache_full;
        "warm_list_over_budget", "1 while 预热列表 repos alone exceed disk_budget", warm_over_budget;
        "evictions", "Repos removed by LRU 淘汰", evictions;
        "blob_cache_hits", "Blob LRU hits", blob_hits;
        "blob_cache_misses", "Blob LRU misses", blob_misses;
        "blob_cache_bytes", "Bytes held by the blob LRU", blob_bytes;
        "blob_cache_capacity_bytes", "Blob LRU budget", blob_capacity;
        "scan_threads", "Size of the grep scan pool", scan_threads;
        "scan_threads_busy", "Scan pool threads currently scanning a file", scan_busy;
        "git_processes", "git CLI children running for log/show/blame", git_procs;
        "git_processes_limit", "Limit on git CLI children for log/show/blame", git_limit;
    }
    Metrics { registry: r, ..m }
});

pub fn router(config: Arc<ConfigHandle>, cache: Arc<RepoCache>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/status", get(status))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .with_state((config, cache))
}

type St = State<(Arc<ConfigHandle>, Arc<RepoCache>)>;

async fn metrics(State((_, cache)): St) -> impl IntoResponse {
    let m = &*METRICS;
    let queue = cache.clone_queue();
    let running = queue
        .iter()
        .filter(|(_, p)| p.queue_position.is_none())
        .count() as i64;
    m.clones_running.set(running);
    m.clone_queue.set(queue.len() as i64 - running);
    {
        let cooling = cache.cooldowns();
        let mut seen = m.cooled_hosts.lock().unwrap();
        seen.extend(cooling.iter().map(|(h, _)| h.clone()));
        for host in seen.iter() {
            let left = cooling.iter().find(|(h, _)| h == host).map(|c| c.1);
            let l = vec![("host", host.clone())];
            m.host_cooling.get_or_create(&l).set(left.is_some() as i64);
            m.host_cooldown
                .get_or_create(&l)
                .set(left.map_or(0.0, |d| d.as_secs_f64()));
        }
    }
    if let Ok(d) = tokio::task::spawn_blocking(move || cache.disk()).await {
        m.disk_used.set(d.used as i64);
        m.disk_budget.set(d.budget as i64);
        m.disk_free.set(d.free as i64);
        m.disk_min_free.set(d.min_free as i64);
        m.cache_full.set(d.cache_full as i64);
        m.warm_over_budget.set(d.warm_list_over_budget as i64);
    }
    let (bytes, capacity, threads) = crate::exec::grep::stats();
    m.blob_bytes.set(bytes as i64);
    m.blob_capacity.set(capacity as i64);
    m.scan_threads.set(threads as i64);
    let (busy, limit) = crate::exec::git_processes();
    m.git_procs.set(busy as i64);
    m.git_limit.set(limit as i64);

    let mut text = String::new();
    encode(&mut text, &m.registry).expect("writing to a String");
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        text,
    )
}

async fn status(State((_, cache)): St) -> Json<serde_json::Value> {
    let queue: Vec<_> = cache
        .clone_queue()
        .into_iter()
        .map(|(url, p)| serde_json::json!({"url": url, "state": p.state, "queue_position": p.queue_position}))
        .collect();
    let cooldowns: Vec<_> = cache
        .cooldowns()
        .into_iter()
        .map(|(host, left)| serde_json::json!({"host": host, "remaining_seconds": left.as_secs()}))
        .collect();
    let disk = tokio::task::spawn_blocking(move || cache.disk()).await.ok();
    Json(serde_json::json!({"clone_queue": queue, "cooldowns": cooldowns, "disk": disk}))
}

/// Config loaded (it is, or we would not be running), cache dir writable, git runnable.
/// 冷却 and `cache_full` deliberately don't count (spec §6.3).
async fn readyz(State((_, cache)): St) -> StatusCode {
    let git = cache.git().command(None).arg("--version").output().await;
    let git_ok = git.is_ok_and(|o| o.status.success());
    if git_ok && cache.writable() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// `git --version` output is at least 2.33 (spec §6.2).
pub fn git_version_ok(out: &str) -> bool {
    let Some(v) = out.trim().strip_prefix("git version ") else {
        return false;
    };
    let mut n = v.split('.').map(|p| p.parse::<u32>().ok());
    matches!((n.next(), n.next()), (Some(Some(major)), Some(Some(minor))) if (major, minor) >= (2, 33))
}
