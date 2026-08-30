//! Local, path-scoped Model Context Protocol server for rxls.
//!
//! The server accepts newline-delimited MCP messages over standard input and
//! output. Workbook tools operate only below explicitly configured roots and
//! keep spreadsheet bytes inside the local process.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

mod a1;
mod model;
mod server;
mod transport;

pub use server::{RxlsMcpServer, ServerConfig};
pub use transport::{BoundedLineReader, MAX_RPC_LINE_BYTES};

/// Maximum bytes accepted for one open workbook.
pub const MAX_WORKBOOK_BYTES: usize = 32 * 1024 * 1024;
/// Maximum current workbook bytes retained across all sessions.
pub const MAX_SESSION_BYTES: usize = 128 * 1024 * 1024;
/// Maximum concurrently open workbook sessions.
pub const MAX_SESSIONS: usize = 4;
/// Maximum cells returned by one range read.
pub const MAX_RANGE_CELLS: usize = 10_000;
/// Maximum cell edits accepted by one atomic MCP call.
pub const MAX_BATCH_EDITS: usize = 100;
/// Maximum serialized payload returned by one tool call.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
