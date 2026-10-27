//! 领域错误: codes, their HTTP/CLI mappings, and the wire body (spec §2.5, §3.1).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidArgument,
    IsDirectory,
    Unauthenticated,
    /// Repo missing, unauthorized, or outside allowlist/token scope; deliberately indistinguishable.
    NotFound,
    PathNotFound,
    RevNotFound,
    RepoWarming,
    CacheFull,
    UpstreamUnavailable,
    DeadlineExceeded,
    Internal,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 11] = [
        Self::InvalidArgument,
        Self::IsDirectory,
        Self::Unauthenticated,
        Self::NotFound,
        Self::PathNotFound,
        Self::RevNotFound,
        Self::RepoWarming,
        Self::CacheFull,
        Self::UpstreamUnavailable,
        Self::DeadlineExceeded,
        Self::Internal,
    ];

    pub fn http_status(self) -> u16 {
        match self {
            Self::InvalidArgument | Self::IsDirectory => 400,
            Self::Unauthenticated => 401,
            Self::NotFound | Self::PathNotFound | Self::RevNotFound => 404,
            Self::Internal => 500,
            Self::UpstreamUnavailable => 502,
            Self::RepoWarming => 503,
            Self::DeadlineExceeded => 504,
            Self::CacheFull => 507,
        }
    }

    /// CLI process exit code.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::InvalidArgument | Self::IsDirectory => 2,
            Self::NotFound | Self::PathNotFound | Self::RevNotFound => 3,
            Self::Unauthenticated => 4,
            Self::RepoWarming | Self::CacheFull | Self::UpstreamUnavailable => 5,
            Self::DeadlineExceeded | Self::Internal => 6,
        }
    }
}

/// `{"error":{"code","message"[,"progress"]}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                code,
                message: message.into(),
                progress: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorDetail {
    pub code: ErrorCode,
    pub message: String,
    /// Only on `repo_warming`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<WarmingProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WarmingProgress {
    pub state: WarmingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WarmingState {
    Queued,
    Cloning,
    RateLimited,
}
