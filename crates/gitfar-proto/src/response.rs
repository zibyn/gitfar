//! The response object shared by all primitives (spec §2.2).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `M` is the primitive-specific metadata, flattened into the top-level object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Response<M> {
    /// Text body mimicking the local command's output.
    pub text: String,
    /// Full 40-hex SHA the whole request was pinned to.
    pub resolved_commit: String,
    pub truncated: bool,
    /// Present only when `truncated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated_reason: Option<TruncatedReason>,
    /// Opaque 续读游标; present only when `truncated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(flatten)]
    pub meta: M,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TruncatedReason {
    /// Hit the item/line limit.
    Limit,
    /// Hit the ~64 KB byte cap on `text`.
    Bytes,
    /// Hit the request deadline.
    Deadline,
}

/// Primitives without extra metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoMeta {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadMeta {
    /// Absent for binary files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<u64>,
    /// Binary files return empty `text` with `binary: true` and `size`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GrepMeta {
    /// Blobs over 10 MB skipped because `skip_large` was on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_large: Vec<String>,
}

pub type ReadResponse = Response<ReadMeta>;
pub type GrepResponse = Response<GrepMeta>;
pub type ListResponse = Response<NoMeta>;
pub type LogResponse = Response<NoMeta>;
pub type ShowResponse = Response<NoMeta>;
pub type BlameResponse = Response<NoMeta>;
