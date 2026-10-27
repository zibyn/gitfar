//! Server configuration (spec §4.1): TOML loading, validation, and SIGHUP hot reload.

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use serde::{Deserialize, Deserializer};

use crate::authz::canonicalize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub cache: Cache,
    #[serde(default)]
    pub upstream: Upstream,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub warm_list: Vec<String>,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub client_tokens: Vec<ClientToken>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: SocketAddr,
    #[serde(default = "default_admin_listen")]
    pub admin_listen: SocketAddr,
    pub cache_dir: PathBuf,
    #[serde(default)]
    pub public_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// 0 = number of cores.
    #[serde(default)]
    pub scan_threads: usize,
    #[serde(default = "secs::<30>", deserialize_with = "duration")]
    pub request_deadline: Duration,
    #[serde(default)]
    pub log_format: LogFormat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    #[serde(deserialize_with = "bytes")]
    pub disk_budget: u64,
    #[serde(default = "default_disk_min_free", deserialize_with = "bytes")]
    pub disk_min_free: u64,
    /// `None` = "auto" (1/4 of physical memory).
    #[serde(default, deserialize_with = "blob_lru")]
    pub blob_lru: Option<u64>,
    #[serde(default = "default_blob_lru_max_object", deserialize_with = "bytes")]
    pub blob_lru_max_object: u64,
    #[serde(default = "secs::<60>", deserialize_with = "duration")]
    pub branch_ttl: Duration,
    #[serde(default = "secs::<20>", deserialize_with = "duration")]
    pub warm_wait: Duration,
    #[serde(default = "secs::<5>", deserialize_with = "duration")]
    pub fetch_timeout: Duration,
    #[serde(default = "default_clone_concurrency")]
    pub clone_concurrency: usize,
    #[serde(default = "default_maintenance_window")]
    pub maintenance_window: MaintenanceWindow,
}

/// Daily window in local minutes since midnight; `start > end` wraps past midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceWindow {
    pub start_minute: u16,
    pub end_minute: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    #[serde(default = "default_host_concurrency")]
    pub host_concurrency: usize,
    #[serde(default = "secs::<30>", deserialize_with = "duration")]
    pub cooldown_initial: Duration,
    #[serde(default = "secs::<900>", deserialize_with = "duration")]
    pub cooldown_max: Duration,
    #[serde(default = "secs::<300>", deserialize_with = "duration")]
    pub negative_cache_ttl: Duration,
}

impl Default for Upstream {
    fn default() -> Self {
        Upstream {
            host_concurrency: default_host_concurrency(),
            cooldown_initial: secs::<30>(),
            cooldown_max: secs::<900>(),
            negative_cache_ttl: secs::<300>(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub prefix: String,
    /// Resolved value: `env:`/`file:` references are already dereferenced.
    pub token: Secret,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientToken {
    pub id: String,
    /// `sha256:<64 hex>`; redacted because the spec bars hashes from logs too.
    pub hash: Secret,
    /// `None` = global.
    pub scope: Option<Vec<String>>,
    pub expires_at: Option<jiff::Timestamp>,
}

/// A string that never shows up in `Debug`/`Display`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError(msg.into())
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| err(format!("cannot read config {}: {e}", path.display())))?;
        Config::from_toml_str(&src)
    }

    pub fn from_toml_str(src: &str) -> Result<Config, ConfigError> {
        let mut c: Config = toml::from_str(src).map_err(|e| {
            // Not `e.to_string()`: that quotes the source line, which may hold a literal credential.
            let line = e.span().map(|s| src[..s.start].lines().count().max(1));
            match line {
                Some(l) => err(format!("config line {l}: {}", e.message().trim())),
                None => err(format!("config: {}", e.message().trim())),
            }
        })?;
        let mut ids = HashSet::new();
        for t in &c.client_tokens {
            if !ids.insert(t.id.as_str()) {
                return Err(err(format!("duplicate client_tokens id {:?}", t.id)));
            }
            if !is_sha256_hash(t.hash.expose()) {
                return Err(err(format!(
                    "client_tokens {:?}: hash must be \"sha256:<64 lowercase hex>\"",
                    t.id
                )));
            }
        }
        for cred in &mut c.credentials {
            cred.token = resolve_secret(&cred.token)
                .map_err(|e| err(format!("credentials {:?}: {e}", cred.prefix)))?;
        }
        canonicalize_all(&mut c.allowlist, "allowlist")?;
        canonicalize_all(&mut c.warm_list, "warm_list")?;
        for t in &mut c.client_tokens {
            if let Some(scope) = &mut t.scope {
                canonicalize_all(scope, &format!("client_tokens {:?} scope", t.id))?;
            }
        }
        let mut prefixes = HashSet::new();
        for cred in &mut c.credentials {
            cred.prefix =
                canonicalize(&cred.prefix).map_err(|e| err(format!("credentials: {e}")))?;
            // One URL maps to exactly one credential (spec §4.4).
            if !prefixes.insert(cred.prefix.clone()) {
                return Err(err(format!(
                    "duplicate credentials prefix {:?}",
                    cred.prefix
                )));
            }
        }
        Ok(c)
    }
}

fn canonicalize_all(prefixes: &mut [String], key: &str) -> Result<(), ConfigError> {
    for p in prefixes {
        *p = canonicalize(p).map_err(|e| err(format!("{key}: {e}")))?;
    }
    Ok(())
}

fn is_sha256_hash(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn resolve_secret(raw: &Secret) -> Result<Secret, String> {
    let raw = raw.expose();
    if let Some(var) = raw.strip_prefix("env:") {
        std::env::var(var)
            .map(Secret)
            .map_err(|_| format!("environment variable {var} is not set"))
    } else if let Some(path) = raw.strip_prefix("file:") {
        std::fs::read_to_string(path)
            .map(|s| Secret(s.trim_end_matches(['\r', '\n']).to_string()))
            .map_err(|e| format!("cannot read {path}: {e}"))
    } else {
        Ok(Secret(raw.to_string()))
    }
}

/// Live config for the request path. Restart-only keys keep their startup values.
pub struct ConfigHandle {
    path: PathBuf,
    current: ArcSwap<Config>,
}

impl ConfigHandle {
    pub fn load(path: impl Into<PathBuf>) -> Result<ConfigHandle, ConfigError> {
        let path = path.into();
        let config = Config::load(&path)?;
        Ok(ConfigHandle {
            path,
            current: ArcSwap::from_pointee(config),
        })
    }

    /// Snapshot for one request; stays consistent even if a reload lands mid-request.
    pub fn current(&self) -> Arc<Config> {
        self.current.load_full()
    }

    /// Re-read the file (SIGHUP). On failure the old config stays and an error is logged.
    pub fn reload(&self) -> Result<(), ConfigError> {
        let mut new = Config::load(&self.path).inspect_err(|e| {
            tracing::error!(error = %e, "config reload failed; keeping previous config");
        })?;
        let old = self.current();
        keep_restart_only(&old, &mut new);
        self.current.store(Arc::new(new));
        tracing::info!("config reloaded");
        Ok(())
    }
}

fn keep_restart_only(old: &Config, new: &mut Config) {
    macro_rules! keep {
        ($($field:ident).+) => {
            if old.$($field).+ != new.$($field).+ {
                tracing::warn!(key = stringify!($($field).+), "changing this key requires a restart; ignored");
                new.$($field).+ = old.$($field).+.clone();
            }
        };
    }
    keep!(server.listen);
    keep!(server.admin_listen);
    keep!(server.cache_dir);
    keep!(server.scan_threads);
    keep!(server.log_format);
    keep!(cache.blob_lru);
}

// ---- value parsers ----

fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let s = String::deserialize(d)?;
    parse_duration(&s).map_err(serde::de::Error::custom)
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("invalid duration {s:?} (expected e.g. \"30s\", \"15m\")");
    let sd: jiff::SignedDuration = s.parse().map_err(|_| bad())?;
    Duration::try_from(sd).map_err(|_| bad())
}

fn bytes<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    parse_bytes(&s).map_err(serde::de::Error::custom)
}

fn blob_lru<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let s = String::deserialize(d)?;
    if s == "auto" {
        return Ok(None);
    }
    parse_bytes(&s).map(Some).map_err(serde::de::Error::custom)
}

fn parse_bytes(s: &str) -> Result<u64, String> {
    let bad = || format!("invalid byte size {s:?} (expected e.g. \"500GB\", \"8MiB\")");
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let mult: u64 = match unit.trim() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return Err(bad()),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(bad)
}

impl<'de> Deserialize<'de> for MaintenanceWindow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let bad = || {
            serde::de::Error::custom(format!(
                "invalid maintenance_window {s:?} (expected \"HH:MM-HH:MM\")"
            ))
        };
        let hm = |p: &str| -> Option<u16> {
            let (h, m) = p.split_once(':')?;
            let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
            (h < 24 && m < 60).then_some(h * 60 + m)
        };
        let (a, b) = s.split_once('-').ok_or_else(bad)?;
        Ok(MaintenanceWindow {
            start_minute: hm(a).ok_or_else(bad)?,
            end_minute: hm(b).ok_or_else(bad)?,
        })
    }
}

fn secs<const N: u64>() -> Duration {
    Duration::from_secs(N)
}

fn default_admin_listen() -> SocketAddr {
    ([127, 0, 0, 1], 9090).into()
}

fn default_disk_min_free() -> u64 {
    10_000_000_000
}

fn default_blob_lru_max_object() -> u64 {
    8_000_000
}

fn default_clone_concurrency() -> usize {
    2
}

fn default_host_concurrency() -> usize {
    4
}

fn default_maintenance_window() -> MaintenanceWindow {
    MaintenanceWindow {
        start_minute: 3 * 60,
        end_minute: 5 * 60,
    }
}
