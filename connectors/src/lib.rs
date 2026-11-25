//! Readers of the sessions of AI tools (Claude Code, Codex, the Claude app, ...).
//!
//! A [`Tool`] knows where its session files live and how to turn one line of such a
//! file into a [`Record`]; its I/O is listing those files and reading their lines
//! ([`for_each_line`]). The Claude app keeps its sessions in a database instead:
//! [`copy_claude_app_cache`] copies it and [`read_claude_app_cache`] reads the copy. The crate
//! depends on neither the agent nor an async runtime.

mod chatgpt;
mod claude_app;
mod claude_app_records;
mod claude_code;
mod codex;
mod indexeddb;
mod leveldb;
mod lines;
mod parsed;
mod record;
mod snapshot;
mod text;
mod tool;
mod v8;

pub use chatgpt::ChatGpt;
pub use claude_app::{claude_app_cache, copy_claude_app_cache, read_claude_app_cache};
pub use claude_app_records::{CLAUDE_APP, ClaudeApp, claude_app_conversation, claude_app_folders};
pub use claude_code::ClaudeCode;
pub use codex::Codex;
pub use indexeddb::{Error as CacheError, StoredValue, ValueError};
pub use lines::{Line, MAX_LINE_BYTES, for_each_line};
pub use parsed::Parsed;
pub use record::{
    Author, Mark, Record, Role, ToolCall, ToolResult, Turn, compare_times, minute_of,
};
pub use snapshot::{Conversation, Identified, Snapshot};
pub use tool::{FileState, Listing, Registry, Tool, tools};
