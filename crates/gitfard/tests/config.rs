use std::time::Duration;

use gitfard::config::{Config, LogFormat};

const HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// The §4.1 example, with a real-shaped hash and a literal credential.
fn example() -> String {
    format!(
        r#"
allowlist = ["github.com/acme", "gitlab.example.com/platform"]
warm_list = ["github.com/acme/monorepo"]

[server]
listen = "0.0.0.0:8080"
admin_listen = "127.0.0.1:9090"
cache_dir = "/var/lib/gitfar"
public_hosts = ["gitfar.example.com"]
allowed_origins = []
scan_threads = 0
request_deadline = "30s"
log_format = "json"

[cache]
disk_budget = "500GB"
disk_min_free = "10GB"
blob_lru = "auto"
blob_lru_max_object = "8MB"
branch_ttl = "60s"
warm_wait = "20s"
fetch_timeout = "5s"
clone_concurrency = 2
maintenance_window = "03:00-05:00"

[upstream]
host_concurrency = 4
cooldown_initial = "30s"
cooldown_max = "15m"
negative_cache_ttl = "5m"

[[credentials]]
prefix = "github.com/acme"
token = "ghp_literalsecret"

[[client_tokens]]
id = "ci-bot"
hash = "{HASH}"
scope = ["github.com/acme"]
expires_at = "2027-01-01T00:00:00Z"
"#
    )
}

#[test]
fn spec_example_loads() {
    let c = Config::from_toml_str(&example()).unwrap();
    assert_eq!(c.server.listen.to_string(), "0.0.0.0:8080");
    assert_eq!(c.server.request_deadline, Duration::from_secs(30));
    assert_eq!(c.server.log_format, LogFormat::Json);
    assert_eq!(c.cache.disk_budget, 500_000_000_000);
    assert_eq!(c.cache.blob_lru, None);
    assert_eq!(c.cache.blob_lru_max_object, 8_000_000);
    assert_eq!(c.upstream.cooldown_max, Duration::from_secs(15 * 60));
    assert_eq!(c.cache.maintenance_window.start_minute, 3 * 60);
    assert_eq!(c.cache.maintenance_window.end_minute, 5 * 60);
    assert_eq!(
        c.allowlist,
        ["github.com/acme", "gitlab.example.com/platform"]
    );
    assert_eq!(c.credentials[0].token.expose(), "ghp_literalsecret");
    assert_eq!(c.client_tokens[0].id, "ci-bot");
    assert_eq!(
        c.client_tokens[0].scope.as_deref(),
        Some(&["github.com/acme".to_string()][..])
    );
    assert_eq!(
        c.client_tokens[0].expires_at.unwrap().to_string(),
        "2027-01-01T00:00:00Z"
    );
}

fn load_err(src: &str) -> String {
    Config::from_toml_str(src).unwrap_err().to_string()
}

#[test]
fn missing_required_key_names_it() {
    let e = load_err(&example().replace("cache_dir = \"/var/lib/gitfar\"\n", ""));
    assert!(e.contains("missing field `cache_dir`"), "{e}");
}

#[test]
fn duplicate_token_id_is_rejected() {
    let dup = format!(
        "{}\n[[client_tokens]]\nid = \"ci-bot\"\nhash = \"{HASH}\"\n",
        example()
    );
    let e = load_err(&dup);
    assert!(e.contains("duplicate client_tokens id \"ci-bot\""), "{e}");
}

#[test]
fn bad_duration_is_rejected_with_line() {
    let e = load_err(&example().replace("\"30s\"\nlog_format", "\"30 parsecs\"\nlog_format"));
    assert!(e.contains("invalid duration \"30 parsecs\""), "{e}");
    assert!(e.starts_with("config line 12:"), "{e}");
}

#[test]
fn bad_byte_size_is_rejected() {
    let e = load_err(&example().replace("\"500GB\"", "\"lots\""));
    assert!(e.contains("invalid byte size \"lots\""), "{e}");
}

#[test]
fn minimal_config_takes_defaults() {
    let c = Config::from_toml_str(
        "[server]\nlisten = \"0.0.0.0:8080\"\ncache_dir = \"/c\"\n[cache]\ndisk_budget = \"1GiB\"\n",
    )
    .unwrap();
    assert_eq!(c.cache.disk_budget, 1 << 30);
    assert_eq!(c.cache.branch_ttl, Duration::from_secs(60));
    assert_eq!(c.server.admin_listen.to_string(), "127.0.0.1:9090");
    assert!(c.client_tokens.is_empty());
}

#[test]
fn credential_from_env_and_file() {
    std::env::set_var("GITFARD_TEST_CRED_ENV", "ghp_fromenv");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tok");
    std::fs::write(&file, "ghp_fromfile\n").unwrap();
    let src = example().replace(
        "token = \"ghp_literalsecret\"",
        &format!(
            "token = \"env:GITFARD_TEST_CRED_ENV\"\n[[credentials]]\nprefix = \"b\"\ntoken = \"file:{}\"",
            file.display()
        ),
    );
    let c = Config::from_toml_str(&src).unwrap();
    assert_eq!(c.credentials[0].token.expose(), "ghp_fromenv");
    assert_eq!(c.credentials[1].token.expose(), "ghp_fromfile");
}

#[test]
fn unset_env_credential_is_an_error() {
    let e = load_err(&example().replace("ghp_literalsecret", "env:GITFARD_TEST_DEFINITELY_UNSET"));
    assert!(
        e.contains("credentials \"github.com/acme\": environment variable GITFARD_TEST_DEFINITELY_UNSET is not set"),
        "{e}"
    );
}

#[test]
fn secrets_never_appear_in_debug_output() {
    let c = Config::from_toml_str(&example()).unwrap();
    let dbg = format!("{c:?} {c:#?}");
    assert!(!dbg.contains("ghp_literalsecret"), "{dbg}");
    assert!(!dbg.contains("000000000000"), "{dbg}");
    assert!(dbg.contains("ci-bot"));
}

#[test]
fn secrets_never_appear_in_parse_errors() {
    let src = "credentials = [{ prefix = \"x\", token = \"ghp_inline\", bogus = 1 }]\n[server]\nlisten = \"0.0.0.0:1\"\ncache_dir = \"/c\"\n[cache]\ndisk_budget = \"1GB\"\n";
    assert!(Config::from_toml_str(src)
        .unwrap_err()
        .to_string()
        .contains("bogus"));
    let e = Config::from_toml_str(src).unwrap_err();
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains("ghp_inline"), "{shown}");
}

#[test]
fn shipped_example_config_loads() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/gitfard.toml");
    let c = Config::load(std::path::Path::new(path)).unwrap();
    assert_eq!(c.server.cache_dir, std::path::Path::new("/var/lib/gitfar"));
}
