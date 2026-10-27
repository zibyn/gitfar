//! `gitfard token new` (spec §4.2): offline issuing of a 客户端访问令牌.

use rand::RngExt;
use sha2::{Digest, Sha256};

pub struct NewToken {
    /// Shown once; never stored.
    pub token: String,
    /// A `[[client_tokens]]` entry ready to paste into the config.
    pub entry: String,
}

#[derive(serde::Serialize)]
struct Entry<'a> {
    id: &'a str,
    hash: String,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    scope: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<jiff::Timestamp>,
}

/// Empty `scope` = global.
pub fn new_token(id: &str, scope: &[String], expires_at: Option<jiff::Timestamp>) -> NewToken {
    // Alphanumeric is exactly the base62 alphabet; 43 chars carry 256 bits, i.e. 32 random bytes.
    let raw: String = rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(43)
        .map(char::from)
        .collect();
    let token = format!("gfr_{raw}");
    let hash = format!("sha256:{}", hex::encode(Sha256::digest(token.as_bytes())));
    let body = toml::to_string(&Entry {
        id,
        hash,
        scope,
        expires_at,
    })
    .expect("entry serializes");
    NewToken {
        token,
        entry: format!("[[client_tokens]]\n{body}"),
    }
}
