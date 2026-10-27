use gitfar_proto::ErrorCode;
use gitfard::authz::{authorize, canonicalize, prefix_matches};
use gitfard::config::{Config, Credential};

fn config(extra: &str) -> Result<Config, gitfard::config::ConfigError> {
    Config::from_toml_str(&format!(
        "{extra}\n[server]\nlisten = \"127.0.0.1:0\"\ncache_dir = \"/c\"\n[cache]\ndisk_budget = \"1GB\"\n"
    ))
}

const CREDS: &str = r#"
allowlist = ["github.com/acme", "gitlab.example.com"]
[[credentials]]
prefix = "github.com/acme"
token = "org"
[[credentials]]
prefix = "github.com/acme/secret"
token = "team"
"#;

fn token_of(r: Result<Option<&Credential>, ErrorCode>) -> Option<&str> {
    r.unwrap().map(|c| c.token.expose())
}

#[test]
fn authorize_picks_the_longest_matching_credential() {
    let c = config(CREDS).unwrap();
    assert_eq!(
        token_of(authorize(&c, None, "github.com/acme/secret/x")),
        Some("team")
    );
    assert_eq!(
        token_of(authorize(&c, None, "github.com/acme/secret")),
        Some("team")
    );
    assert_eq!(
        token_of(authorize(&c, None, "github.com/acme/secretive")),
        Some("org")
    );
    assert_eq!(
        token_of(authorize(&c, None, "github.com/acme/x")),
        Some("org")
    );
}

#[test]
fn authorize_treats_allowlisted_repo_without_credential_as_public() {
    let c = config(CREDS).unwrap();
    assert_eq!(
        token_of(authorize(&c, None, "gitlab.example.com/g/p")),
        None
    );
}

#[test]
fn authorize_rejects_outside_allowlist_as_not_found() {
    let c = config(CREDS).unwrap();
    // Credentials alone never grant access: the allowlist gates first.
    for url in ["github.com/acme-evil/x", "github.com/other", "github.com"] {
        assert_eq!(
            authorize(&c, None, url).unwrap_err(),
            ErrorCode::NotFound,
            "{url}"
        );
    }
}

#[test]
fn authorize_enforces_token_scope() {
    let c = config(CREDS).unwrap();
    let scope = ["github.com/acme/secret".to_string()];
    assert_eq!(
        token_of(authorize(&c, Some(&scope), "github.com/acme/secret/x")),
        Some("team")
    );
    assert_eq!(
        authorize(&c, Some(&scope), "github.com/acme/x").unwrap_err(),
        ErrorCode::NotFound
    );
    assert_eq!(
        authorize(&c, Some(&[]), "github.com/acme/x").unwrap_err(),
        ErrorCode::NotFound
    );
}

#[test]
fn config_prefixes_are_canonicalised_on_load() {
    let c = config(
        r#"
allowlist = ["https://GitHub.com/acme/"]
warm_list = ["git@github.com:acme/mono.git"]
[[credentials]]
prefix = "https://bot@github.com/acme.git"
token = "t"
[[client_tokens]]
id = "ci"
hash = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
scope = ["HTTPS://GITHUB.COM/acme/x"]
"#,
    )
    .unwrap();
    assert_eq!(c.allowlist, ["github.com/acme"]);
    assert_eq!(c.warm_list, ["github.com/acme/mono"]);
    assert_eq!(c.credentials[0].prefix, "github.com/acme");
    let scope = c.client_tokens[0].scope.as_deref().unwrap();
    assert_eq!(scope, ["github.com/acme/x"]);
    assert_eq!(
        token_of(authorize(&c, Some(scope), "github.com/acme/x")),
        Some("t")
    );
}

#[test]
fn config_rejects_invalid_or_duplicate_prefixes() {
    for bad in [
        "allowlist = [\"github.com/a/../b\"]",
        "warm_list = [\"\"]",
        "[[credentials]]\nprefix = \"github.com/a\"\ntoken = \"x\"\n\
         [[credentials]]\nprefix = \"https://github.com/a.git\"\ntoken = \"y\"",
    ] {
        assert!(config(bad).is_err(), "{bad}");
    }
}

#[test]
fn prefix_matches_only_on_path_segment_boundaries() {
    assert!(prefix_matches("github.com/acme", "github.com/acme"));
    assert!(prefix_matches("github.com/acme", "github.com/acme/x"));
    assert!(prefix_matches("github.com", "github.com/acme/x"));
    assert!(!prefix_matches("github.com/acme", "github.com/acme-evil"));
    assert!(!prefix_matches("github.com/acme", "github.com/acme-evil/x"));
    assert!(!prefix_matches("github.com/acme/x", "github.com/acme"));
    assert!(!prefix_matches("github.co", "github.com/acme"));
}

#[test]
fn canonicalize_normalizes_to_host_path() {
    for (raw, want) in [
        ("https://github.com/acme/repo", "github.com/acme/repo"),
        ("http://github.com/acme/repo", "github.com/acme/repo"),
        (
            "https://user:pw@github.com/acme/repo",
            "github.com/acme/repo",
        ),
        ("https://GitHub.COM/Acme/Repo", "github.com/Acme/Repo"),
        ("https://github.com/acme/repo.git", "github.com/acme/repo"),
        ("https://github.com/acme/repo.git/", "github.com/acme/repo"),
        ("https://github.com/acme/repo/", "github.com/acme/repo"),
        ("git@github.com:acme/repo.git", "github.com/acme/repo"),
        ("git@GitHub.com:Acme/Repo", "github.com/Acme/Repo"),
        ("ssh://git@github.com/acme/repo.git", "github.com/acme/repo"),
        ("github.com/acme/repo", "github.com/acme/repo"),
        ("github.com/acme", "github.com/acme"),
        ("github.com", "github.com"),
    ] {
        assert_eq!(canonicalize(raw).as_deref(), Ok(want), "{raw}");
    }
}

#[test]
fn canonicalize_rejects_malformed_urls() {
    for raw in [
        "",
        "https://",
        "https:///acme/repo",
        "github.com/acme/../evil",
        "github.com/acme/./repo",
        "github.com//acme",
        "github.com/acme repo",
        "github.com/acme?x=1",
        "github.com/acme#frag",
        "github.com\\acme",
    ] {
        assert!(canonicalize(raw).is_err(), "{raw:?} should be rejected");
    }
}
