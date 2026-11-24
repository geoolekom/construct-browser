//! Readers of the session logs of coding tools (Claude Code, Codex, ...).
//!
//! A [`Tool`] knows where its session files live and how to turn one line of such a
//! file into a [`Record`]. The crate's I/O is listing those files and reading their lines
//! ([`for_each_line`]); it depends on neither the agent nor an async runtime.

mod chatgpt;
mod claude_code;
mod codex;
mod lines;
mod parsed;
mod record;
mod text;
mod tool;

pub use chatgpt::ChatGpt;
pub use claude_code::ClaudeCode;
pub use codex::Codex;
pub use lines::{Line, MAX_LINE_BYTES, for_each_line};
pub use parsed::Parsed;
pub use record::{
    Author, Mark, Record, Role, ToolCall, ToolResult, Turn, compare_times, minute_of,
};
pub use tool::{FileState, Listing, Registry, Tool, tools};
