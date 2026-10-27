//! Types shared by gitfard, the gitfar CLI and the MCP tools (spec §2, §7).

mod cursor;
mod error;
mod params;
mod primitive;
mod response;

pub use cursor::*;
pub use error::*;
pub use params::*;
pub use primitive::*;
pub use response::*;
