//! 续读游标: `base64url(version byte ‖ JSON {primitive, params, resolved_commit, offset})`
//! (spec §2.4, ADR 0004). Unsigned and stateless; the server re-authorizes every resume.

use crate::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    /// Full 40-hex SHA; resumes skip rev resolution and run on it directly.
    pub resolved_commit: String,
    pub resume: Resume,
}

/// Original params plus the primitive's own offset; pairing them per variant keeps
/// a grep offset from ever riding on a log request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "primitive", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resume {
    /// 1-based line to emit next.
    Read { params: ReadParams, next_line: u32 },
    /// Last emitted (path, line); resume strictly after it in path-then-line order.
    Grep {
        params: GrepParams,
        path: String,
        line: u32,
    },
    /// Entries already emitted.
    List { params: ListParams, skipped: u64 },
    /// Commits already emitted.
    Log { params: LogParams, skipped: u64 },
    /// Bytes of output text already emitted.
    Show {
        params: ShowParams,
        byte_offset: u64,
    },
    /// 1-based line to emit next.
    Blame { params: BlameParams, next_line: u32 },
}

impl Resume {
    pub fn primitive(&self) -> Primitive {
        match self {
            Self::Read { .. } => Primitive::Read,
            Self::Grep { .. } => Primitive::Grep,
            Self::List { .. } => Primitive::List,
            Self::Log { .. } => Primitive::Log,
            Self::Show { .. } => Primitive::Show,
            Self::Blame { .. } => Primitive::Blame,
        }
    }
}

/// A fresh request's params for any primitive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    Read(ReadParams),
    Grep(GrepParams),
    List(ListParams),
    Log(LogParams),
    Show(ShowParams),
    Blame(BlameParams),
}

/// A primitive request body: either fresh params or exactly `{"cursor": "…"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Fresh(Params),
    Resume(Cursor),
}

impl Request {
    /// Parses the body sent to `primitive` (HTTP JSON body / MCP tool arguments).
    /// Every failure is `invalid_argument`.
    pub fn parse(primitive: Primitive, body: serde_json::Value) -> Result<Self, ErrorBody> {
        let invalid = |why: String| ErrorBody::new(ErrorCode::InvalidArgument, why);
        if let Some(token) = body.get("cursor") {
            if body.as_object().is_some_and(|o| o.len() != 1) {
                return Err(invalid("cursor must be sent alone".into()));
            }
            let token = token
                .as_str()
                .ok_or_else(|| invalid("cursor must be a string".into()))?;
            let cursor = Cursor::decode(token)?;
            if cursor.resume.primitive() != primitive {
                return Err(invalid(format!(
                    "cursor belongs to {}",
                    cursor.resume.primitive().name()
                )));
            }
            return Ok(Self::Resume(cursor));
        }
        fn from<P: serde::de::DeserializeOwned>(v: serde_json::Value) -> serde_json::Result<P> {
            serde_json::from_value(v)
        }
        let params = match primitive {
            Primitive::Read => from(body).map(Params::Read),
            Primitive::Grep => from(body).map(Params::Grep),
            Primitive::List => from(body).map(Params::List),
            Primitive::Log => from(body).map(Params::Log),
            Primitive::Show => from(body).map(Params::Show),
            Primitive::Blame => from(body).map(Params::Blame),
        };
        params.map(Self::Fresh).map_err(|e| invalid(e.to_string()))
    }
}

impl Cursor {
    pub fn encode(&self) -> String {
        let mut bytes = vec![VERSION];
        serde_json::to_writer(&mut bytes, self).expect("cursor serializes");
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Any failure is `invalid_argument`.
    pub fn decode(token: &str) -> Result<Self, ErrorBody> {
        let invalid = |why: &str| ErrorBody::new(ErrorCode::InvalidArgument, why);
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| invalid("cursor is not valid base64url"))?;
        let (&version, body) = bytes.split_first().ok_or_else(|| invalid("empty cursor"))?;
        if version != VERSION {
            return Err(invalid("unsupported cursor version"));
        }
        let cursor: Self = serde_json::from_slice(body).map_err(|_| invalid("malformed cursor"))?;
        // Trust boundary: the SHA reaches gix and the git CLI unresolved.
        let c = &cursor.resolved_commit;
        if c.len() != 40 || !c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid("malformed cursor"));
        }
        Ok(cursor)
    }
}
