//! RPC data layer for the MCP stdio transport.
//!
//! - [`framing`] — newline-delimited JSON I/O over async readers/writers.
//! - [`message`] — typed helpers for classifying and extracting MCP JSON-RPC
//!   messages.

pub mod framing;
pub mod message;
pub mod pipeline;

// Re-export the most commonly used types and functions at the module level so
// callers can write `use crate::rpc::{FrameReader, MessageKind, ...}`.
pub use framing::{FrameReader, read_line, write_line, write_raw_line};
pub use message::{
    MessageKind, ToolSchema, extract_resources_read_uri, extract_tool_schemas, extract_tools_call,
    message_id, message_kind, replace_in_strings,
};
