use std::time::Duration;

use gitfard::git::{GitError, GitFailure};

/// What `git clone` prints under `GIT_TRACE_CURL=1 GIT_TRACE_CURL_NO_DATA=1` for an HTTP error.
fn traced(status: &str, retry_after: Option<&str>) -> String {
    let mut s = String::from(
        "09:41:22.118204 http.c:845              == Info: Couldn't find host github.com in the .netrc file; using defaults\n\
         09:41:22.131977 http.c:845              => Send header: GET /acme/x.git/info/refs?service=git-upload-pack HTTP/2\n\
         09:41:22.132001 http.c:845              => Send header: Authorization: Basic <redacted>\n",
    );
    s += &format!("09:41:22.402311 http.c:845              <= Recv header: HTTP/2 {status}\n");
    if let Some(after) = retry_after {
        s += &format!(
            "09:41:22.402330 http.c:845              <= Recv header: retry-after: {after}\n"
        );
    }
    s += &format!(
        "09:41:22.402351 http.c:845              <= Recv header: content-type: text/plain\n\
         fatal: unable to access 'https://github.com/acme/x.git/': The requested URL returned error: {status}\n"
    );
    s
}

fn kind(stderr: &str) -> GitFailure {
    GitError::from_stderr(stderr).kind
}

#[test]
fn rate_limits_carry_retry_after_when_sent() {
    assert_eq!(
        kind(&traced("429", Some("120"))),
        GitFailure::RateLimited {
            retry_after: Some(Duration::from_secs(120))
        }
    );
    assert_eq!(
        kind(&traced("503", None)),
        GitFailure::RateLimited { retry_after: None }
    );
}

#[test]
fn http_date_retry_after_is_ignored() {
    assert_eq!(
        kind(&traced("429", Some("Wed, 21 Oct 2026 07:28:00 GMT"))),
        GitFailure::RateLimited { retry_after: None }
    );
}

#[test]
fn not_found_in_both_shapes() {
    assert_eq!(kind(&traced("404", None)), GitFailure::NotFound);
    assert_eq!(
        kind("remote: Repository not found.\nfatal: repository 'https://github.com/acme/x.git/' not found\n"),
        GitFailure::NotFound
    );
}

#[test]
fn auth_failures() {
    for stderr in [
        traced("401", None),
        traced("403", None),
        "remote: Invalid username or token.\nfatal: Authentication failed for 'https://github.com/acme/x.git/'\n".into(),
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n".into(),
    ] {
        assert_eq!(kind(&stderr), GitFailure::AuthFailed, "{stderr}");
    }
}

#[test]
fn rate_limit_wins_over_not_found_which_wins_over_auth() {
    let both = format!("{}{}", traced("404", None), traced("429", None));
    assert!(matches!(kind(&both), GitFailure::RateLimited { .. }));
    let both = format!("{}{}", traced("403", None), traced("404", None));
    assert_eq!(kind(&both), GitFailure::NotFound);
}

#[test]
fn anything_else_is_other() {
    assert_eq!(
        kind("fatal: unable to access 'https://github.com/acme/x.git/': Could not resolve host: github.com\n"),
        GitFailure::Other
    );
}

#[test]
fn curl_trace_never_reaches_stderr() {
    let e = GitError::from_stderr(&traced("429", Some("5")));
    assert_eq!(
        e.stderr,
        "fatal: unable to access 'https://github.com/acme/x.git/': The requested URL returned error: 429"
    );
    assert!(!e.to_string().contains("Authorization"));
}
