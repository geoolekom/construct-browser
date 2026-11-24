use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::Deserialize;

use crate::text::{
    add_tool_name, argv_command, join_texts, plain_string, plain_strings, root_under,
};
use crate::{
    Author, FileState, Line as FileLine, Listing, Mark, Parsed, Record, Role, Tool, Turn,
    for_each_line,
};

/// The app that writes a session as a chat when it has the ChatGPT app's own workspace.
const DESKTOP_ORIGINATOR: &str = "Codex Desktop";

/// Which of the session files in the Codex sessions directory a reader takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sessions {
    /// Every session but the chats of the ChatGPT app.
    Codex,
    /// The chats of the ChatGPT app. Their folder is one the app made for the chat, not a
    /// project, so no record carries it.
    ChatGpt,
}

impl Sessions {
    /// The name of the tool these sessions are stored under.
    pub(crate) fn tool(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ChatGpt => "chatgpt",
        }
    }
}

/// Which sessions each file belongs to, by path, with the length the file had when it was
/// read. The first line of a session file never changes once it is written, so each file is
/// read for it once; a file that is shorter than it was is a new file at the same path, as the
/// follower takes it, and is read again. A file no listing finds any more is forgotten.
pub(crate) static BELONGS: LazyLock<Mutex<HashMap<PathBuf, (Sessions, u64)>>> =
    LazyLock::new(Mutex::default);

/// The tool's own context that Codex writes as user messages. A text block that starts
/// with one of these is not something the user typed.
const CONTEXT_MARKERS: [&str; 11] = [
    "<environment_context>",
    "<recommended_plugins>",
    "<user_instructions>",
    "<INSTRUCTIONS>",
    "<permissions instructions>",
    "<collaboration_mode>",
    "<skills_instructions>",
    "<turn_aborted>",
    "<user_shell_command>",
    "<subagent_notification>",
    "# AGENTS.md instructions",
];

/// Codex: `<home>/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl`, where `<home>` is
/// `$CODEX_HOME` or `~/.codex`.
///
/// The folder and branch are written once, at the start of a file; the session id is also
/// in the file name. All three are kept in the [`FileState`], so a read that starts in the
/// middle of a file knows them.
///
/// The chats of the ChatGPT app are in the same place and are left to
/// [`ChatGpt`](crate::ChatGpt).
pub struct Codex;

impl Codex {
    fn root_in(home: &Path, codex_home: Option<OsString>) -> PathBuf {
        root_under(home, codex_home, ".codex", "sessions")
    }
}

/// The sessions directory of Codex for `home`, also the one of the ChatGPT app.
pub(crate) fn sessions_root(home: &Path) -> PathBuf {
    Codex::root_in(home, env::var_os("CODEX_HOME"))
}

/// The files of `sessions` under `root`.
pub(crate) fn find_sessions(root: &Path, sessions: Sessions) -> io::Result<Listing> {
    let mut listing = Listing::default();
    collect_files(root, 0, &mut listing)?;
    BELONGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|path, _| !path.starts_with(root) || listing.files.contains(path));
    listing
        .files
        .retain(|path| sessions_of(path) == Some(sessions));
    listing.files.sort();
    Ok(listing)
}

/// The sessions a file belongs to. A file whose first line is not complete yet belongs to
/// none for now, so no reader takes it before it is known whose it is. A file that cannot be
/// read is left to Codex, whose read reports it.
fn sessions_of(path: &Path) -> Option<Sessions> {
    let Ok(len) = fs::metadata(path).map(|meta| meta.len()) else {
        return Some(Sessions::Codex);
    };
    let known = BELONGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .copied();
    if let Some((sessions, read_at)) = known
        && read_at <= len
    {
        return Some(sessions);
    }
    let mut first = None;
    let read = for_each_line(path, 0, |line, _| {
        first = Some(line);
        false
    });
    if read.is_err() {
        return Some(Sessions::Codex);
    }
    let sessions = if is_chat_line(&first?) {
        Sessions::ChatGpt
    } else {
        Sessions::Codex
    };
    BELONGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_owned(), (sessions, len));
    Some(sessions)
}

/// Whether the first line of a session file names a chat of the ChatGPT app.
fn is_chat_line(line: &FileLine) -> bool {
    let FileLine::Text(text) = line else {
        return false;
    };
    serde_json::from_str::<Line>(text)
        .ok()
        .filter(|line| line.kind == "session_meta")
        .and_then(|line| line.payload)
        .is_some_and(|meta| meta.is_chat())
}

/// The ChatGPT app's own workspace, where it makes a folder for each chat without a project.
fn is_chat_workspace(path: &Path) -> bool {
    path.ends_with(Path::new("Documents").join("Codex"))
}

/// The text blocks of a message of `role`, one after the other, without the tool's own
/// context in user messages; `None` when nothing is left.
fn join_text(role: Role, blocks: &[Block]) -> Option<String> {
    let wanted = match role {
        Role::User => "input_text",
        Role::Assistant => "output_text",
    };
    join_texts(
        blocks
            .iter()
            .filter(|b| b.kind == wanted)
            .filter_map(|b| b.text.as_deref())
            .filter(|t| role == Role::Assistant || !is_context(t)),
    )
}

fn is_context(text: &str) -> bool {
    let text = text.trim_start();
    CONTEXT_MARKERS.iter().any(|m| text.starts_with(m))
}

/// The id of a session from a file name `rollout-<time>-<uuid>.jsonl`.
fn session_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.strip_prefix("rollout-")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    let shaped = id.bytes().enumerate().all(|(i, b)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            b == b'-'
        } else {
            b.is_ascii_hexdigit()
        }
    });
    shaped.then(|| id.to_owned())
}

/// The command line of a shell call, from the JSON text of its arguments: `command` as a
/// string or a list of words, or `cmd`. Nothing else of the arguments is read.
fn shell_command(arguments: &str) -> Option<String> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Command {
        Line(String),
        Words(Vec<String>),
    }
    #[derive(Deserialize)]
    struct Arguments {
        command: Option<Command>,
        cmd: Option<String>,
    }
    let args = serde_json::from_str::<Arguments>(arguments).ok()?;
    match args.command {
        Some(Command::Line(line)) => Some(line),
        Some(Command::Words(words)) => argv_command(&words).map(str::to_owned),
        None => args.cmd,
    }
}

/// The name of the tool a `function_call` or `custom_tool_call` line asks to run.
fn tool_names(line_kind: &str, payload: &Payload) -> Vec<String> {
    let mut tools = Vec::new();
    if line_kind == "response_item"
        && matches!(
            payload.kind.as_deref(),
            Some("function_call" | "custom_tool_call")
        )
        && let Some(name) = payload.name.as_deref()
    {
        let command = payload.arguments.as_deref().and_then(shell_command);
        add_tool_name(&mut tools, name, command.as_deref());
    }
    tools
}

fn collect_files(dir: &Path, depth: u8, listing: &mut Listing) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                listing
                    .problems
                    .push(format!("cannot list {}: {e}", dir.display()));
                continue;
            }
        };
        if path.is_dir() {
            if depth < 3 {
                // A day directory that cannot be read is named; the other days are still read.
                if let Err(e) = collect_files(&path, depth + 1, listing) {
                    listing
                        .problems
                        .push(format!("cannot list {}: {e}", path.display()));
                }
            }
        } else if path.is_file()
            && path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
        {
            listing.files.push(path);
        }
    }
    Ok(())
}

impl Tool for Codex {
    fn name(&self) -> &'static str {
        Sessions::Codex.tool()
    }

    fn root(&self, home: &Path) -> PathBuf {
        sessions_root(home)
    }

    fn find_files(&self, root: &Path) -> io::Result<Listing> {
        find_sessions(root, Sessions::Codex)
    }

    fn open_file(&self, path: &Path) -> FileState {
        open_session(path)
    }

    fn parse_line(&self, state: &mut FileState, line: &str) -> Parsed {
        parse_session_line(Sessions::Codex, state, line)
    }
}

/// The state of a session file that was never read: the session id of its name.
pub(crate) fn open_session(path: &Path) -> FileState {
    FileState {
        session: session_of(path).unwrap_or_default(),
        ..FileState::default()
    }
}

/// Reads one line of a Codex session file into a record of `sessions`.
pub(crate) fn parse_session_line(sessions: Sessions, state: &mut FileState, line: &str) -> Parsed {
    // A file read before as Codex is resumed with the folder Codex saved in its state.
    if sessions == Sessions::ChatGpt {
        state.folder.clear();
    }
    let Ok(line) = serde_json::from_str::<Line>(line) else {
        return Parsed::Skipped;
    };
    let payload = line.payload.unwrap_or_default();
    match line.kind.as_str() {
        "session_meta" => {
            if state.session.is_empty() {
                state.session = payload.id.unwrap_or_default();
            }
            if sessions == Sessions::Codex
                && let Some(cwd) = payload.cwd.filter(|c| !c.is_empty())
            {
                state.folder = cwd;
            }
            if let Some(branch) = payload.git.and_then(|g| g.branch) {
                state.branch = branch;
            }
            let source = payload.source;
            state.author = if source.as_deref() == Some("exec")
                || payload.originator.as_deref() == Some("codex_exec")
            {
                Author::Unattended
            } else if source.is_some() {
                Author::Person
            } else {
                Author::Unknown
            };
            return Parsed::Ignored;
        }
        "turn_context" => {
            if sessions == Sessions::Codex
                && let Some(cwd) = payload.cwd.filter(|c| !c.is_empty())
            {
                state.folder = cwd;
            }
            return Parsed::Ignored;
        }
        "response_item" | "event_msg" => {}
        _ => return Parsed::Ignored,
    }
    let Some(time) = line.timestamp else {
        return Parsed::Ignored;
    };
    let role = match (
        line.kind.as_str(),
        payload.kind.as_deref(),
        payload.role.as_deref(),
    ) {
        ("response_item", Some("message"), Some("user")) => Some(Role::User),
        ("response_item", Some("message"), Some("assistant")) => Some(Role::Assistant),
        ("response_item", Some("message"), Some("developer")) => return Parsed::Ignored,
        _ => None,
    };
    let tools = tool_names(&line.kind, &payload);
    let text = role.and_then(|r| join_text(r, &payload.content.unwrap_or_default()));
    match (role, text) {
        (Some(role), Some(text)) => Parsed::Record(Record::Turn(Turn {
            tool: sessions.tool().into(),
            session: state.session.clone(),
            time,
            folder: state.folder.clone(),
            branch: state.branch.clone(),
            role,
            author: if role == Role::User {
                state.author
            } else {
                Author::Unknown
            },
            text,
            tools,
        })),
        _ => Parsed::Record(Record::Mark(Mark {
            tool: sessions.tool().into(),
            session: state.session.clone(),
            time,
            folder: state.folder.clone(),
            branch: state.branch.clone(),
            tools,
        })),
    }
}

/// The few fields of a line that are read; the rest (instructions, tool outputs, ...) is
/// skipped while parsing and never held in memory.
#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
    payload: Option<Payload>,
}

#[derive(Default, Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    git: Option<Git>,
    /// `session_meta` only: where the session was started (`cli`, `exec`, ...). A source
    /// that is not a plain string (a sub-agent) is read as no source.
    #[serde(default, deserialize_with = "plain_string")]
    source: Option<String>,
    #[serde(default, deserialize_with = "plain_string")]
    originator: Option<String>,
    role: Option<String>,
    content: Option<Vec<Block>>,
    /// `function_call` and `custom_tool_call`: the tool's name, and for a function the
    /// JSON text of its arguments, of which only a shell command is read.
    #[serde(default, deserialize_with = "plain_string")]
    name: Option<String>,
    #[serde(default, deserialize_with = "plain_string")]
    arguments: Option<String>,
    /// `session_meta` only: the folders the session may work in.
    #[serde(default, deserialize_with = "plain_strings")]
    runtime_workspace_roots: Option<Vec<String>>,
}

impl Payload {
    /// A `session_meta` of a chat of the ChatGPT app: started by the desktop app in a folder
    /// it made inside its own workspace, with only that workspace to work in. A session with a
    /// project chosen in the app, the workspace itself included, stays Codex. Without the list
    /// of workspaces, the folder of the session decides.
    fn is_chat(&self) -> bool {
        if self.originator.as_deref() != Some(DESKTOP_ORIGINATOR) {
            return false;
        }
        let Some(cwd) = self.cwd.as_deref().map(Path::new) else {
            return false;
        };
        let inside = |workspace: &Path| {
            is_chat_workspace(workspace)
                && cwd
                    .strip_prefix(workspace)
                    .is_ok_and(|rest| rest.components().next().is_some())
        };
        match self.runtime_workspace_roots.as_deref() {
            Some(roots) if !roots.is_empty() => roots.iter().all(|root| inside(Path::new(root))),
            _ => cwd.ancestors().skip(1).any(is_chat_workspace),
        }
    }
}

#[derive(Deserialize)]
struct Git {
    branch: Option<String>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: String,
    text: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const ID: &str = "01a0bee2-6260-73a3-b389-43c86bb8930f";
    const T: &str = "2026-11-11T10:15:30.123Z";

    fn path() -> PathBuf {
        PathBuf::from(format!(
            "/h/.codex/sessions/2026/11/11/rollout-2026-11-11T10-15-00-{ID}.jsonl"
        ))
    }

    pub(crate) fn item(kind: &str, payload: &str) -> String {
        format!(r#"{{"timestamp":"{T}","type":"{kind}","payload":{payload}}}"#)
    }

    pub(crate) fn message(role: &str, block: &str, text: &str) -> String {
        item(
            "response_item",
            &format!(
                r#"{{"type":"message","role":"{role}","content":[{{"type":"{block}","text":"{text}"}}]}}"#
            ),
        )
    }

    fn meta(extra: &str) -> String {
        item(
            "session_meta",
            &format!(
                r#"{{"id":"{ID}","cwd":"/w/app","originator":"cli","base_instructions":{{"text":"long"}}{extra}}}"#
            ),
        )
    }

    /// A state after the first lines of a file were read.
    fn started(meta_line: &str) -> FileState {
        let mut st = Codex.open_file(&path());
        assert_eq!(Codex.parse_line(&mut st, meta_line), Parsed::Ignored);
        st
    }

    pub(crate) fn turn(p: Parsed) -> Turn {
        match p {
            Parsed::Record(Record::Turn(t)) => t,
            other => panic!("not a turn: {other:?}"),
        }
    }

    fn assert_mark(p: &Parsed, folder: &str, branch: &str) {
        let Parsed::Record(Record::Mark(m)) = p else {
            panic!("not a mark: {p:?}")
        };
        assert_eq!(
            (m.session.as_str(), m.folder.as_str(), m.branch.as_str()),
            (ID, folder, branch)
        );
        assert_eq!((m.tool.as_str(), m.time.as_str()), ("codex", T));
    }

    #[test]
    fn session_meta_with_git() {
        let st = started(&meta(r#","git":{"commit_hash":"abc","branch":"feat"}"#));
        assert_eq!(
            (st.session.as_str(), st.folder.as_str(), st.branch.as_str()),
            (ID, "/w/app", "feat")
        );
    }

    #[test]
    fn session_meta_gives_the_session_only_when_the_file_name_does_not() {
        let mut st = Codex.open_file(Path::new("/x/rollout-bad.jsonl"));
        assert_eq!(Codex.parse_line(&mut st, &meta("")), Parsed::Ignored);
        assert_eq!(st.session, ID);
        let other = meta("").replace(ID, "ffffffff-0000-0000-0000-000000000000");
        let mut named = Codex.open_file(&path());
        assert_eq!(Codex.parse_line(&mut named, &other), Parsed::Ignored);
        assert_eq!(named.session, ID);
    }

    #[test]
    fn session_meta_without_git() {
        for extra in ["", r#","git":{"commit_hash":"abc"}"#] {
            let st = started(&meta(extra));
            assert_eq!(
                (st.session.as_str(), st.folder.as_str(), st.branch.as_str()),
                (ID, "/w/app", "")
            );
        }
    }

    #[test]
    fn user_and_assistant_messages_are_turns() {
        let mut st = started(&meta(r#","git":{"branch":"feat"}"#));
        let u = turn(Codex.parse_line(&mut st, &message("user", "input_text", "fix it\\nnow")));
        assert_eq!(
            (u.role, u.text.as_str(), u.time.as_str()),
            (Role::User, "fix it\nnow", T)
        );
        assert_eq!(
            (u.session.as_str(), u.folder.as_str(), u.branch.as_str()),
            (ID, "/w/app", "feat")
        );
        let a = turn(Codex.parse_line(&mut st, &message("assistant", "output_text", "Done.")));
        assert_eq!(
            (a.role, a.text.as_str(), a.tool.as_str()),
            (Role::Assistant, "Done.", "codex")
        );
    }

    #[test]
    fn developer_message_is_nothing() {
        let mut st = started(&meta(""));
        assert_eq!(
            Codex.parse_line(&mut st, &message("developer", "input_text", "rules")),
            Parsed::Ignored
        );
    }

    #[test]
    fn the_tools_own_context_is_a_mark_only() {
        let mut st = started(&meta(""));
        for marker in CONTEXT_MARKERS {
            let text = format!("{marker}\\nrest of the block");
            let p = Codex.parse_line(&mut st, &message("user", "input_text", &text));
            assert_mark(&p, "/w/app", "");
        }
        let two = item(
            "response_item",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"},{"type":"input_text","text":"the real prompt"}]}"#,
        );
        assert_eq!(
            turn(Codex.parse_line(&mut st, &two)).text,
            "the real prompt"
        );
    }

    #[test]
    fn reasoning_tool_calls_and_events_are_marks_only() {
        let mut st = started(&meta(""));
        for p in [
            r#"{"type":"reasoning","summary":[],"encrypted_content":"AAAA"}"#,
            r#"{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}"#,
            r#"{"type":"function_call_output","call_id":"c1","output":"a very long output"}"#,
        ] {
            assert_mark(
                &Codex.parse_line(&mut st, &item("response_item", p)),
                "/w/app",
                "",
            );
        }
        let ev = item("event_msg", r#"{"type":"agent_message","message":"hi"}"#);
        assert_mark(&Codex.parse_line(&mut st, &ev), "/w/app", "");
    }

    #[test]
    fn other_lines_are_ignored_and_turn_context_updates_the_folder() {
        let mut st = started(&meta(""));
        assert_eq!(
            Codex.parse_line(&mut st, &item("token_usage_record", "{}")),
            Parsed::Ignored
        );
        let tc = item("turn_context", r#"{"cwd":"/w/other","model":"m"}"#);
        assert_eq!(Codex.parse_line(&mut st, &tc), Parsed::Ignored);
        assert_eq!(st.folder, "/w/other");
    }

    #[test]
    fn unparseable_line_is_skipped() {
        let mut st = Codex.open_file(&path());
        assert_eq!(
            Codex.parse_line(&mut st, r#"{"timestamp":"2026-11-11T10:1"#),
            Parsed::Skipped
        );
        assert_eq!(Codex.parse_line(&mut st, "not json"), Parsed::Skipped);
    }

    fn user_author_of(session_meta: &str) -> Author {
        let mut st = Codex.open_file(&path());
        Codex.parse_line(&mut st, &item("session_meta", session_meta));
        turn(Codex.parse_line(&mut st, &message("user", "input_text", "go"))).author
    }

    fn user_author(meta_extra: &str) -> Author {
        let mut st = started(&meta(meta_extra));
        let t = turn(Codex.parse_line(&mut st, &message("user", "input_text", "go")));
        let a = turn(Codex.parse_line(&mut st, &message("assistant", "output_text", "ok")));
        assert_eq!(a.author, Author::Unknown);
        t.author
    }

    #[test]
    fn the_session_source_tells_who_wrote_the_prompts() {
        assert_eq!(user_author(r#","source":"exec""#), Author::Unattended);
        assert_eq!(user_author(r#","source":"cli""#), Author::Person);
        assert_eq!(user_author(r#","source":"vscode""#), Author::Person);
        assert_eq!(user_author(""), Author::Unknown);
        // Either mark of an unattended run is enough, whatever the other says.
        assert_eq!(
            user_author_of(r#"{"id":"x","source":"cli","originator":"codex_exec"}"#),
            Author::Unattended
        );
        // An originator that is not a string is no mark.
        assert_eq!(
            user_author_of(r#"{"id":"x","source":"cli","originator":7}"#),
            Author::Person
        );
        let exec = r#"{"id":"x","originator":"codex_exec"}"#;
        let mut st = Codex.open_file(&path());
        Codex.parse_line(&mut st, &item("session_meta", exec));
        let t = turn(Codex.parse_line(&mut st, &message("user", "input_text", "go")));
        assert_eq!(t.author, Author::Unattended);
        // A source that is not a plain string is no source: never guessed.
        assert_eq!(
            user_author(r#","source":{"subagent":"review"}"#),
            Author::Unknown
        );
    }

    #[test]
    fn a_session_meta_with_odd_marks_still_gives_its_session() {
        let mut st = Codex.open_file(&path());
        let odd = r#"{"id":"x","source":7,"originator":["x"],"git":{"branch":"feat"}}"#;
        assert_eq!(
            Codex.parse_line(&mut st, &item("session_meta", odd)),
            Parsed::Ignored
        );
        assert_eq!((st.session.as_str(), st.branch.as_str()), (ID, "feat"));
        assert_eq!(st.author, Author::Unknown);
    }

    #[test]
    fn the_author_survives_a_restart_in_the_middle_of_a_file() {
        let mut first = started(&meta(r#","source":"exec""#));
        let saved = serde_json::to_string(&first).unwrap();
        let _ = Codex.parse_line(&mut first, &message("user", "input_text", "one"));
        let mut restored: FileState = serde_json::from_str(&saved).unwrap();
        let t = turn(Codex.parse_line(&mut restored, &message("user", "input_text", "two")));
        assert_eq!(t.author, Author::Unattended);
        // A state saved before the field existed reads as unknown.
        let old: FileState =
            serde_json::from_str(r#"{"session":"s","folder":"/f","branch":""}"#).unwrap();
        assert_eq!(old.author, Author::Unknown);
    }

    #[test]
    fn a_read_from_the_middle_uses_the_saved_state() {
        let mut first = started(&meta(r#","git":{"branch":"feat"}"#));
        let saved = serde_json::to_string(&first).unwrap();
        assert!(matches!(
            Codex.parse_line(&mut first, &message("user", "input_text", "one")),
            Parsed::Record(_)
        ));
        // The agent restarts and reads on from its saved offset: no session_meta in sight.
        let mut restored: FileState = serde_json::from_str(&saved).unwrap();
        let t = turn(Codex.parse_line(&mut restored, &message("assistant", "output_text", "two")));
        assert_eq!(
            (t.session.as_str(), t.folder.as_str(), t.branch.as_str()),
            (ID, "/w/app", "feat")
        );
        // A file never read, entered in the middle: the id still comes from the file name.
        let mut fresh = Codex.open_file(&path());
        let t = turn(Codex.parse_line(&mut fresh, &message("user", "input_text", "three")));
        assert_eq!((t.session.as_str(), t.folder.as_str()), (ID, ""));
    }

    #[test]
    fn session_id_is_taken_from_the_file_name() {
        assert_eq!(Codex.open_file(&path()).session, ID);
        assert_eq!(
            Codex.open_file(Path::new("/x/rollout-bad.jsonl")).session,
            ""
        );
    }

    #[test]
    fn finds_rollout_files_and_honours_codex_home() {
        let dir = std::env::temp_dir().join(format!("codex-find-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let day = dir.join("sessions/2026/11/11");
        fs::create_dir_all(&day).unwrap();
        fs::write(
            day.join(format!("rollout-2026-11-11T10-15-00-{ID}.jsonl")),
            meta("") + "\n",
        )
        .unwrap();
        fs::write(day.join("other.jsonl"), meta("") + "\n").unwrap();
        let files = Codex.find_files(&dir.join("sessions")).unwrap().files;
        assert_eq!(files.len(), 1);
        assert!(
            Codex
                .find_files(&dir.join("missing"))
                .unwrap()
                .files
                .is_empty()
        );
        let home = Path::new("/h");
        assert_eq!(Codex::root_in(home, None), Path::new("/h/.codex/sessions"));
        assert_eq!(
            Codex::root_in(home, Some("/cx".into())),
            Path::new("/cx/sessions")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_day_directory_that_cannot_be_listed_is_named_and_the_other_days_are_still_listed() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("codex-listing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (bad, good) = (root.join("2026/11/10"), root.join("2026/11/11"));
        fs::create_dir_all(&bad).unwrap();
        fs::create_dir_all(&good).unwrap();
        let log = good.join(format!("rollout-2026-11-11T10-15-00-{ID}.jsonl"));
        fs::write(&log, meta("") + "\n").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();
        let locked = fs::read_dir(&bad).is_err();
        let listing = Codex.find_files(&root).unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&root);
        if !locked {
            return;
        }
        assert_eq!(listing.files, [log]);
        assert_eq!(listing.problems.len(), 1);
        assert!(
            listing.problems[0].starts_with(&format!("cannot list {}", bad.display())),
            "{:?}",
            listing.problems
        );
    }

    fn call(kind: &str, name: &str, arguments: &str) -> String {
        item(
            "response_item",
            &format!(
                r#"{{"type":"{kind}","name":"{name}","arguments":{},"call_id":"c1"}}"#,
                serde_json::to_string(arguments).unwrap()
            ),
        )
    }

    fn call_tools(l: &str) -> Vec<String> {
        let mut state = Codex.open_file(&path());
        match Codex.parse_line(&mut state, l) {
            Parsed::Record(Record::Mark(m)) => m.tools,
            other => panic!("not a mark: {other:?}"),
        }
    }

    #[test]
    fn the_names_of_function_calls_are_on_the_mark_and_arguments_are_not_kept() {
        let plain = call("function_call", "update_plan", r#"{"plan":"hunter2"}"#);
        assert_eq!(call_tools(&plain), ["update_plan"]);
        let custom = call("custom_tool_call", "apply_patch", "*** secret patch");
        assert_eq!(call_tools(&custom), ["apply_patch"]);
        let words = call(
            "function_call",
            "shell",
            r#"{"command":["bash","-lc","gh pr list --token hunter2"]}"#,
        );
        assert_eq!(call_tools(&words), ["bash:gh"]);
        let program = call(
            "function_call",
            "shell",
            r#"{"command":["glab","mr","list"]}"#,
        );
        assert_eq!(call_tools(&program), ["bash:glab"]);
        let cmd = call("function_call", "exec_command", r#"{"cmd":"git status"}"#);
        assert_eq!(call_tools(&cmd), ["bash:git"]);
        let odd = call("function_call", "shell", "not json");
        assert_eq!(call_tools(&odd), ["shell"]);
    }

    #[test]
    fn a_call_output_and_other_lines_give_no_names() {
        let output = item(
            "response_item",
            r#"{"type":"function_call_output","call_id":"c1","name":"shell","output":"shell"}"#,
        );
        assert!(call_tools(&output).is_empty());
        let event = item("event_msg", r#"{"type":"exec_command_end","name":"shell"}"#);
        assert!(call_tools(&event).is_empty());
        // The same call under another line type is not a call.
        let call_in_event = item(
            "event_msg",
            r#"{"type":"function_call","name":"shell","arguments":"{}"}"#,
        );
        assert!(call_tools(&call_in_event).is_empty());
    }

    #[test]
    fn every_shell_tool_name_gives_the_program_from_a_command_string_too() {
        for name in ["shell", "shell_command", "exec_command", "container.exec"] {
            let line = call("function_call", name, r#"{"command":"gh pr list"}"#);
            assert_eq!(call_tools(&line), ["bash:gh"], "{name}");
        }
        let other = call(
            "function_call",
            "update_plan",
            r#"{"command":"gh pr list"}"#,
        );
        assert_eq!(call_tools(&other), ["update_plan"]);
    }
}
