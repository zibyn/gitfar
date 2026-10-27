//! Parameters of the six 检索原语 (spec §2.1, §2.3).
//!
//! Defaults and caps (e.g. `read` 200/2000 lines) are applied by the executor,
//! so optional knobs stay `Option` here and round-trip unchanged through cursors.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `read`: `cat -n` style view of one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadParams {
    /// Repository URL in any form; normalised server-side.
    pub repo: String,
    /// git revision; defaults to `HEAD` (upstream default branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
}

/// `grep`: Rust `regex` search over the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepParams {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Rust `regex` syntax (no backtracking, no multiline).
    pub pattern: String,
    /// Path prefixes relative to the repo root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// git pathspec-style globs (`*` stays within `/`, `**` spans dirs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_string: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_case: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Defaults to `lines`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<GrepOutputMode>,
    /// Defaults to 100 matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
    /// Skip blobs over 10 MB; defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_large: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    Lines,
    Files,
    Count,
}

/// `list`: covers `ls` and `find`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// Defaults to `any`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<ListType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListType {
    File,
    Dir,
    Any,
}

/// `log`: one line per commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogParams {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// Defaults to 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep: Option<String>,
}

/// `show`: one commit (header + diff) or an `a..b` range (diff only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowParams {
    pub repo: String,
    /// Required: a single rev or an `a..b` range.
    pub rev: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// Defaults to `patch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ShowFormat>,
    /// Defaults to 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lines: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ShowFormat {
    Stat,
    NameOnly,
    Patch,
}

/// `blame`: line range is required, at most 500 lines (enforced by the executor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameParams {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
}
