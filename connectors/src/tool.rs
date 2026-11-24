use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Author, ChatGpt, ClaudeCode, Codex, Parsed};

/// What a reader remembers about one file between lines. It can be saved and restored,
/// so reading continues after a restart in the middle of a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub session: String,
    pub folder: String,
    /// Empty when unknown.
    pub branch: String,
    /// Who wrote the prompts of the whole file, when the tool says it once per session.
    #[serde(default)]
    pub author: Author,
}

/// The session files of a tool, and what could not be listed on the way.
#[derive(Debug, Default)]
pub struct Listing {
    pub files: Vec<PathBuf>,
    /// One line for each directory or entry that could not be read.
    pub problems: Vec<String>,
}

/// A coding tool whose session files are read.
pub trait Tool: Send + Sync {
    /// Short stable name; it is the `tool` of every record and a directory name on disk.
    fn name(&self) -> &'static str;

    /// Where the tool keeps its session files, for a user's home directory.
    fn root(&self, home: &Path) -> PathBuf;

    /// The session files under `root`. A directory below `root` that cannot be read is left
    /// out of the files and named in [`Listing::problems`], so one bad directory does not
    /// hide the other files.
    ///
    /// # Errors
    /// Fails when `root` itself cannot be read; a missing `root` is not an error.
    fn find_files(&self, root: &Path) -> io::Result<Listing>;

    /// The state of a file that was never read: what its path tells, such as the session id.
    fn open_file(&self, _path: &Path) -> FileState {
        FileState::default()
    }

    /// Reads one line of a file. `state` carries the session id, folder and branch learned
    /// from earlier lines of the same file. A line of no interest is [`Parsed::Ignored`]; one
    /// that does not parse is [`Parsed::Skipped`], which the caller counts.
    fn parse_line(&self, state: &mut FileState, line: &str) -> Parsed;
}

/// A list of tools.
pub struct Registry(Vec<Box<dyn Tool>>);

impl Registry {
    #[must_use]
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self(tools)
    }

    /// The tools this crate knows.
    #[must_use]
    pub fn builtin() -> Self {
        Self(tools())
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.0.iter().map(AsRef::as_ref)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.iter().find(|t| t.name() == name)
    }
}

/// The tools of this crate. A new tool is one module and one line here.
#[must_use]
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(ClaudeCode), Box::new(Codex), Box::new(ChatGpt)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Author, Record, Role, Turn};

    /// A tool defined outside the crate's modules: its own type and one line in a list.
    struct Dummy;

    impl Tool for Dummy {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn root(&self, home: &Path) -> PathBuf {
            home.join(".dummy")
        }

        fn find_files(&self, root: &Path) -> io::Result<Listing> {
            Ok(Listing {
                files: vec![root.join("s.log")],
                problems: Vec::new(),
            })
        }

        fn parse_line(&self, state: &mut FileState, line: &str) -> Parsed {
            if let Some(id) = line.strip_prefix("session ") {
                state.session = id.to_owned();
                return Parsed::Ignored;
            }
            Parsed::Record(Record::Turn(Turn {
                tool: self.name().into(),
                session: state.session.clone(),
                time: "2026-11-11T10:00:00Z".into(),
                folder: state.folder.clone(),
                branch: state.branch.clone(),
                role: Role::User,
                author: Author::Unknown,
                text: line.to_owned(),
                tools: Vec::new(),
            }))
        }
    }

    #[test]
    fn a_new_tool_is_one_type_and_one_line() {
        let registry = Registry::new(vec![Box::new(Dummy)]);
        let tool = registry.get("dummy").unwrap();
        assert_eq!(tool.root(Path::new("/h")), Path::new("/h/.dummy"));
        assert_eq!(tool.find_files(Path::new("/r")).unwrap().files.len(), 1);
        let mut state = FileState::default();
        assert_eq!(tool.parse_line(&mut state, "session abc"), Parsed::Ignored);
        let saved = serde_json::to_string(&state).unwrap();
        let mut restored: FileState = serde_json::from_str(&saved).unwrap();
        let Parsed::Record(record) = tool.parse_line(&mut restored, "hello") else {
            panic!("not a record");
        };
        assert_eq!(record.session(), "abc");
        assert!(registry.get("other").is_none());
        assert!(Registry::builtin().get("dummy").is_none());
        assert!(Registry::builtin().get("claude-code").is_some());
        assert!(Registry::builtin().get("codex").is_some());
        assert!(Registry::builtin().get("chatgpt").is_some());
    }
}
