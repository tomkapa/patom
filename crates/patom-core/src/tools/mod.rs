//! Tool subsystem.
//!
//! A `Tool` is anything the model can invoke. Implementations register a name, a
//! human-readable description, a JSON schema for inputs, and an `execute` method that
//! turns a `Value` input into a `String` output (success or error).
//!
//! The agent never imports concrete tools directly — it pulls a [`ToolRegistry`] off the
//! composition root. Adding a system tool is one new file in `system/` plus one
//! `.with(...)` line in [`system::register`].

pub mod limits;
mod modes;
mod pg_recorder;
mod recorder;
mod registry;
pub mod system;
mod toolbox;
mod traits;
mod url;

pub use limits::{
    DEFAULT_TOOL_CALLS_PAGE, MAX_TOOL_CALL_DURATION_MS, MAX_TOOL_CALL_ERROR_MESSAGE_BYTES,
    MAX_TOOL_CALLS_PAGE, MAX_TOOL_NAME_BYTES, TOOL_RESULT_MAX_BYTES, truncate_from_start,
    truncate_to_char_boundary,
};
pub use modes::RequestKindModes;
pub use pg_recorder::{PgToolCallStore, clip_error_message};
pub use recorder::{
    SharedToolCallStore, ToolCallRow, ToolCallRowId, ToolCallStore, ToolCallStoreError,
};
pub use registry::{ToolRegistry, ToolRegistryBuilder};
pub use toolbox::{DynamicToolSource, ToolBox};
pub use traits::{SharedTool, Tool, ToolCallContext, ToolError};
pub use url::{FetchUrl, UrlError};
