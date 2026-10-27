//! The six 检索原语 and their JSON Schemas (for MCP `inputSchema`/`outputSchema`).

use crate::*;
use schemars::{schema_for, JsonSchema, Schema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Primitive {
    Read,
    Grep,
    List,
    Log,
    Show,
    Blame,
}

impl Primitive {
    pub const ALL: [Primitive; 6] = [
        Self::Read,
        Self::Grep,
        Self::List,
        Self::Log,
        Self::Show,
        Self::Blame,
    ];

    /// Wire name: HTTP path segment `/v1/{name}` and MCP tool name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Grep => "grep",
            Self::List => "list",
            Self::Log => "log",
            Self::Show => "show",
            Self::Blame => "blame",
        }
    }

    pub fn params_schema(self) -> Schema {
        match self {
            Self::Read => schema_for!(ReadParams),
            Self::Grep => schema_for!(GrepParams),
            Self::List => schema_for!(ListParams),
            Self::Log => schema_for!(LogParams),
            Self::Show => schema_for!(ShowParams),
            Self::Blame => schema_for!(BlameParams),
        }
    }

    pub fn response_schema(self) -> Schema {
        match self {
            Self::Read => schema_for!(ReadResponse),
            Self::Grep => schema_for!(GrepResponse),
            Self::List => schema_for!(ListResponse),
            Self::Log => schema_for!(LogResponse),
            Self::Show => schema_for!(ShowResponse),
            Self::Blame => schema_for!(BlameResponse),
        }
    }
}
