//! 规范 URL, path-segment prefix matching, and the per-request authorization decision (spec §4.3, §4.4).

use gitfar_proto::ErrorCode;

use crate::config::{Config, Credential};

/// Normalizes a repo URL or config prefix to `host/path`. A bare host (`github.com`) is valid,
/// so the same function serves both request URLs and allowlist/scope/credential/warm-list prefixes.
pub fn canonicalize(raw: &str) -> Result<String, String> {
    let rest = match raw.split_once("://") {
        Some((_, rest)) => rest,
        // scp-like `git@host:path`: a `:` before any `/`.
        None => match raw.split_once(':') {
            Some((host, path)) if !host.contains('/') => {
                return canonicalize(&format!("{host}/{path}")).map_err(|_| bad(raw));
            }
            _ => raw,
        },
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let mut path = path.trim_end_matches('/');
    path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');

    // Reject anything that could make the cache key or prefix match disagree with what git fetches.
    let ok_segment = |s: &str| {
        !matches!(s, "" | "." | "..")
            && !s.chars().any(|c| {
                c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '\\' | '@' | '%')
            })
    };
    if !ok_segment(host) || (!path.is_empty() && !path.split('/').all(ok_segment)) {
        return Err(bad(raw));
    }
    let host = host.to_ascii_lowercase();
    Ok(if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    })
}

/// Whether canonical `url` lies under canonical `prefix`, matching whole path segments only
/// (`github.com/acme` matches `github.com/acme/x`, not `github.com/acme-evil`).
pub fn prefix_matches(prefix: &str, url: &str) -> bool {
    url.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The per-request authorization decision (spec §4.4, ADR 0001). Pure over the current config:
/// it never looks at the cache, so tightening the config hides cached data immediately.
///
/// `url` must be canonical; `scope` is the client token's 令牌范围 (`None` = global).
/// `Ok(Some(cred))` = fetch with the longest-prefix credential, `Ok(None)` = public repo, fetch anonymously.
pub fn authorize<'c>(
    config: &'c Config,
    scope: Option<&[String]>,
    url: &str,
) -> Result<Option<&'c Credential>, ErrorCode> {
    let within = |prefixes: &[String]| prefixes.iter().any(|p| prefix_matches(p, url));
    if !within(&config.allowlist) || !scope.is_none_or(within) {
        return Err(ErrorCode::NotFound);
    }
    Ok(config
        .credentials
        .iter()
        .filter(|c| prefix_matches(&c.prefix, url))
        .max_by_key(|c| c.prefix.len()))
}

fn bad(raw: &str) -> String {
    format!("invalid repository URL {raw:?}")
}
