use std::path::Path;
use std::time::Duration;

use gitfard::config::ConfigHandle;

fn write(path: &Path, listen: &str, ttl: &str, allow: &str) {
    let src = format!(
        "allowlist = [\"{allow}\"]\n[server]\nlisten = \"{listen}\"\ncache_dir = \"/c\"\n\
         [cache]\ndisk_budget = \"1GB\"\nbranch_ttl = \"{ttl}\"\n"
    );
    std::fs::write(path, src).unwrap();
}

#[test]
fn successful_reload_is_seen_by_new_requests_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    write(&path, "0.0.0.0:8080", "60s", "github.com/acme");
    let handle = ConfigHandle::load(&path).unwrap();
    let in_flight = handle.current();

    write(&path, "0.0.0.0:8080", "10s", "github.com/other");
    handle.reload().unwrap();

    let now = handle.current();
    assert_eq!(now.cache.branch_ttl, Duration::from_secs(10));
    assert_eq!(now.allowlist, ["github.com/other"]);
    // A request that took its snapshot before the reload keeps a consistent view.
    assert_eq!(in_flight.allowlist, ["github.com/acme"]);
}

#[test]
fn failed_reload_keeps_old_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    write(&path, "0.0.0.0:8080", "60s", "github.com/acme");
    let handle = ConfigHandle::load(&path).unwrap();

    write(&path, "0.0.0.0:8080", "soon", "github.com/other");
    let e = handle.reload().unwrap_err();
    assert!(e.to_string().contains("invalid duration \"soon\""), "{e}");

    let now = handle.current();
    assert_eq!(now.cache.branch_ttl, Duration::from_secs(60));
    assert_eq!(now.allowlist, ["github.com/acme"]);
}

#[test]
fn restart_only_keys_are_ignored_on_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gitfard.toml");
    write(&path, "0.0.0.0:8080", "60s", "github.com/acme");
    let handle = ConfigHandle::load(&path).unwrap();

    write(&path, "0.0.0.0:9999", "10s", "github.com/acme");
    handle.reload().unwrap();

    let now = handle.current();
    assert_eq!(now.server.listen.to_string(), "0.0.0.0:8080");
    assert_eq!(now.cache.branch_ttl, Duration::from_secs(10));
}
