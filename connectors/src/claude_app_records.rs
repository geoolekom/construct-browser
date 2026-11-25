//! The records of the Claude app's conversations: what a person and the assistant wrote, as
//! turns, and a mark for each step that ran tools. A chat is a tree of messages, of which the
//! branch that ends at the message shown last is read. A Cowork session is a stream of events, of
//! which the user and assistant messages are read. Every record carries an id that tells it from
//! every other record of its session, for a reader that reads the same conversation many times.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::claude_app::{blob_dir, leveldb_dir};
use crate::claude_code::has_harness_tag;
use crate::text::{add_tool_name, join_texts};
use crate::{
    Author, Conversation, Identified, Mark, Record, Role, Snapshot, StoredValue, Turn,
    claude_app_cache, copy_claude_app_cache, read_claude_app_cache,
};

/// The name of the source the Claude app's records are stored under.
pub const CLAUDE_APP: &str = "claude-app";

/// The key prefix of a Cowork session in the cache.
const COWORK_KEY: &str = "cowork:";

/// The file the app names the folders of its Cowork sessions in, one per account and organization.
const SPACES_FILE: &str = "remote-session-spaces.json";

/// The Claude app as a [`Snapshot`]: a read copies the app's cache into a folder of the reader's
/// own, reads the copy and removes it. The cache is not read again until one of its files, or a
/// file that names the folders of Cowork sessions, changes.
pub struct ClaudeApp {
    scratch: PathBuf,
    /// The files as the last read found them.
    last: Option<Vec<Stamp>>,
}

/// A file as a read found it.
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

impl ClaudeApp {
    /// A reader that copies the cache into `scratch`, which only it uses.
    #[must_use]
    pub fn new(scratch: PathBuf) -> Self {
        Self {
            scratch,
            last: None,
        }
    }
}

impl Snapshot for ClaudeApp {
    fn name(&self) -> &'static str {
        CLAUDE_APP
    }

    fn installed(&self, home: &Path) -> bool {
        app_dir(home).is_dir()
    }

    fn read(&mut self, home: &Path) -> Result<Option<Vec<Conversation>>, String> {
        let cache = claude_app_cache(home);
        let mut files = Vec::new();
        for dir in [leveldb_dir(&cache), blob_dir(&cache)] {
            stamp_tree(&dir, &mut files);
        }
        for path in spaces_files(home) {
            stamp(path, &mut files);
        }
        if self.last.as_ref() == Some(&files) {
            return Ok(None);
        }
        let conversations = if leveldb_dir(&cache).is_dir() {
            // The copy holds the person's conversations: it is not kept past the read, also when
            // the read panics, and a copy that cannot be removed is an error, so that it is tried
            // again and seen.
            let read = {
                let _copy = Copy(&self.scratch);
                copy_claude_app_cache(&cache, &self.scratch)
                    .and_then(|()| read_claude_app_cache(&self.scratch))
            };
            remove_copy(&self.scratch)?;
            let folders = claude_app_folders(home)?;
            read.map_err(|e| e.to_string())?
                .iter()
                .map(|value| claude_app_conversation(value, &folders))
                .collect()
        } else {
            Vec::new()
        };
        self.last = Some(files);
        Ok(Some(conversations))
    }

    fn forget(&mut self) {
        self.last = None;
        // A copy that a crash left behind goes with the rest of what is erased.
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

/// The copy of the cache, removed when this goes out of scope, by unwinding too.
struct Copy<'a>(&'a Path);

impl Drop for Copy<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0);
    }
}

fn remove_copy(scratch: &Path) -> Result<(), String> {
    match fs::remove_dir_all(scratch) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!(
            "cannot remove the copy of the cache at {}: {e}",
            scratch.display()
        )),
        _ => Ok(()),
    }
}

/// Every file under `dir`, with its size and change time, in a stable order; none when `dir`
/// cannot be read.
fn stamp_tree(dir: &Path, files: &mut Vec<Stamp>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            stamp_tree(&path, files);
        } else {
            stamp(path, files);
        }
    }
}

fn stamp(path: PathBuf, files: &mut Vec<Stamp>) {
    if let Ok(meta) = fs::metadata(&path) {
        files.push(Stamp {
            len: meta.len(),
            modified: meta.modified().ok(),
            path,
        });
    }
}

/// The files that name the folders of Cowork sessions,
/// `local-agent-mode-sessions/<account>/<organization>/remote-session-spaces.json`, in a stable
/// order. Only these are looked at: the sessions' own folders beside them change all the time.
fn spaces_files(home: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(sessions_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|account| fs::read_dir(account.path()).into_iter().flatten().flatten())
        .map(|org| org.path().join(SPACES_FILE))
        .filter(|file| file.is_file())
        .collect();
    files.sort();
    files
}

/// The folder the app keeps its data in, on macOS.
fn app_dir(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Claude")
}

/// Where the app keeps what it knows of its Cowork sessions, the folders among it.
fn sessions_dir(home: &Path) -> PathBuf {
    app_dir(home).join("local-agent-mode-sessions")
}

/// The folder each Cowork session works in, by the id the cache gives the session without its
/// prefix (`01…` of `cse_01…`), from the app's `remote-session-spaces.json` files. A session
/// given no folder has none.
///
/// # Errors
/// A file that cannot be read, or does not have the shape of the app's list of sessions.
pub fn claude_app_folders(home: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut folders = BTreeMap::new();
    for file in spaces_files(home) {
        let unreadable = |reason: String| format!("cannot read {}: {reason}", file.display());
        let text = fs::read_to_string(&file).map_err(|e| unreadable(e.to_string()))?;
        let spaces: Value = serde_json::from_str(&text).map_err(|e| unreadable(e.to_string()))?;
        let entries = spaces["entries"]
            .as_array()
            .ok_or_else(|| unreadable("no list of sessions".into()))?;
        for entry in entries {
            let id = entry["sessionId"]
                .as_str()
                .and_then(session_suffix)
                .ok_or_else(|| unreadable("a session without its id".into()))?;
            if let Some(folder) = entry["folders"][0].as_str() {
                folders.insert(id.to_owned(), folder.to_owned());
            }
        }
    }
    Ok(folders)
}

/// The id of a session without its kind (`session_01…` and `cse_01…` both give `01…`).
fn session_suffix(id: &str) -> Option<&str> {
    id.split_once('_').map(|(_, rest)| rest)
}

/// One conversation of the cache with its records, or why they cannot be read: a value that
/// cannot be read, or one without the shape of a chat or a Cowork session. `folders` gives the
/// folder of a Cowork session (see [`claude_app_folders`]).
#[must_use]
pub fn claude_app_conversation(
    stored: &StoredValue,
    folders: &BTreeMap<String, String>,
) -> Conversation {
    let session = stored.key.strip_prefix(COWORK_KEY).unwrap_or(&stored.key);
    let records = stored
        .value
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|value| match value["product"].as_str() {
            Some("chat") => chat_records(session, &value["tree"]),
            Some("cowork") => {
                let folder = session_suffix(session)
                    .and_then(|id| folders.get(id))
                    .map_or("", String::as_str);
                cowork_records(session, folder, &value["tree"])
            }
            other => Err(format!("a conversation of the unknown product {other:?}")),
        });
    Conversation {
        session: session.to_owned(),
        records,
    }
}

/// The text blocks of `content` (a string or a list of blocks) one after the other, without the
/// harness's own text in a user message, and the names of the tools its `tool_use` blocks run.
fn texts_and_tools(role: Role, content: &Value) -> (Option<String>, Vec<String>) {
    let own = |text: &&str| role == Role::Assistant || !has_harness_tag(text);
    if let Some(text) = content.as_str() {
        return (join_texts(Some(text).filter(own).into_iter()), Vec::new());
    }
    let blocks = content.as_array().map_or(&[][..], Vec::as_slice);
    let text = join_texts(
        blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .filter(own),
    );
    let mut tools = Vec::new();
    for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
        if let Some(name) = block["name"].as_str() {
            add_tool_name(&mut tools, name, block["input"]["command"].as_str());
        }
    }
    (text, tools)
}

/// Where and when a message was said, and by whom.
struct Message<'a> {
    id: &'a str,
    role: Role,
    author: Author,
    time: String,
    session: &'a str,
    folder: &'a str,
}

/// A turn when there is text, else a mark when tools ran, else nothing.
fn record(message: Message<'_>, content: &Value) -> Option<Identified> {
    let Message {
        id,
        role,
        author,
        time,
        session,
        folder,
    } = message;
    let (text, tools) = texts_and_tools(role, content);
    let record = match text {
        Some(text) => Record::Turn(Turn {
            tool: CLAUDE_APP.into(),
            session: session.into(),
            time,
            folder: folder.into(),
            branch: String::new(),
            role,
            author,
            text,
            tools,
        }),
        None if !tools.is_empty() => Record::Mark(Mark {
            tool: CLAUDE_APP.into(),
            session: session.into(),
            time,
            folder: folder.into(),
            branch: String::new(),
            tools,
        }),
        None => return None,
    };
    Some(Identified {
        id: id.into(),
        record,
    })
}

/// The records of a chat. Anything it lacks of the shape this reader knows is an error, so that a
/// new version of the app that changed it is seen, not read as a chat with fewer messages.
fn chat_records(session: &str, tree: &Value) -> Result<Vec<Identified>, String> {
    let messages = tree["messageByUuid"]
        .as_object()
        .ok_or("a chat without its messages")?;
    if messages.is_empty() {
        return Ok(Vec::new());
    }
    // From the message shown last back to the first one, whose parent is not a message.
    let leaf = tree["current_leaf_message_uuid"]
        .as_str()
        .ok_or("a chat without the message it shows last")?;
    if !messages.contains_key(leaf) {
        return Err("a chat that shows last a message it does not have".into());
    }
    let mut branch = Vec::new();
    let mut next = Some(leaf);
    while let Some((uuid, message)) = next.and_then(|uuid| messages.get_key_value(uuid)) {
        if branch.len() > messages.len() {
            return Err("a chat whose messages form a loop".into());
        }
        branch.push((uuid.as_str(), message));
        next = tree["parentByChildUuid"][uuid].as_str();
    }
    branch.reverse();
    let mut records = Vec::new();
    for (uuid, message) in branch {
        // A chat has no messages but the ones a person types and the answers.
        let (role, author) = match message["sender"].as_str() {
            Some("human") => (Role::User, Author::Person),
            Some("assistant") => (Role::Assistant, Author::Unknown),
            other => return Err(format!("a chat message from the unknown sender {other:?}")),
        };
        let time = message["created_at"]
            .as_str()
            .ok_or("a chat message without its time")?;
        let said = Message {
            id: uuid,
            role,
            author,
            time: time.to_owned(),
            session,
            folder: "",
        };
        records.extend(record(said, &message["content"]));
    }
    Ok(records)
}

/// The records of a Cowork session. As for a chat, a lack of the known shape is an error. Events
/// of other kinds, and messages of other types (the session's system messages, its results), are
/// none of its records.
fn cowork_records(session: &str, folder: &str, tree: &Value) -> Result<Vec<Identified>, String> {
    let events = tree["events"]
        .as_array()
        .ok_or("a Cowork session without its events")?;
    let mut records = Vec::new();
    for event in events {
        if event["kind"]
            .as_str()
            .ok_or("a Cowork event without its kind")?
            != "message"
        {
            continue;
        }
        let payload = &event["payload"];
        // A user message the app itself sent is the person's; the session adds others.
        let (role, author) = match payload["type"].as_str() {
            Some("user") if payload["client_platform"].is_string() => (Role::User, Author::Person),
            Some("user") => (Role::User, Author::Unknown),
            Some("assistant") => (Role::Assistant, Author::Unknown),
            Some(_) => continue,
            None => return Err("a Cowork message without its type".into()),
        };
        // A subagent's messages are not the person's or the session's own.
        if !payload["parent_tool_use_id"].is_null() {
            continue;
        }
        let id = event["dedupKey"]
            .as_str()
            .ok_or("a Cowork message without its id")?;
        let time = event_time(event).ok_or("a Cowork message without its time")?;
        let said = Message {
            id,
            role,
            author,
            time,
            session,
            folder,
        };
        records.extend(record(said, &payload["message"]["content"]));
    }
    Ok(records)
}

/// When the server took an event, from its whole milliseconds since 1970, in RFC 3339.
fn event_time(event: &Value) -> Option<String> {
    let millis = event["serverCreatedAt"].as_i64()?;
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stored(key: &str, value: Value) -> StoredValue {
        StoredValue {
            key: key.into(),
            value: Ok(value),
        }
    }

    fn records_of(
        stored: &StoredValue,
        folders: &BTreeMap<String, String>,
    ) -> Result<Vec<Identified>, String> {
        claude_app_conversation(stored, folders).records
    }

    fn turn(record: &Identified) -> (&str, &str, Role, Author, &str, &str) {
        let Record::Turn(t) = &record.record else {
            panic!("not a turn: {record:?}");
        };
        (
            record.id.as_str(),
            t.session.as_str(),
            t.role,
            t.author,
            t.time.as_str(),
            t.text.as_str(),
        )
    }

    #[test]
    fn a_chat_gives_the_texts_of_the_branch_shown_last_and_a_mark_for_tools() {
        let chat = json!({
            "product": "chat",
            "tree": {
                "current_leaf_message_uuid": "m3",
                "parentByChildUuid": { "m1": "root", "m2": "m1", "m3": "m1" },
                "messageByUuid": {
                    "m1": { "sender": "human", "created_at": "2026-11-24T13:47:00.000000Z",
                            "content": [{ "type": "text", "text": "Tell me about Berlin" }] },
                    "m2": { "sender": "assistant", "created_at": "2026-11-24T13:47:05.000000Z",
                            "content": [{ "type": "text", "text": "an answer left by an edit" }] },
                    "m3": { "sender": "assistant", "created_at": "2026-11-24T13:48:00.000000Z",
                            "content": [
                                { "type": "thinking", "thinking": "plan" },
                                { "type": "text", "text": "Berlin is" },
                                { "type": "tool_use", "name": "web_search", "input": {} },
                                { "type": "tool_result", "content": "results" },
                                { "type": "text", "text": "the capital." }
                            ] }
                }
            }
        });
        let records = records_of(&stored("chat-1", chat), &BTreeMap::new()).unwrap();
        assert_eq!(
            records.iter().map(turn).collect::<Vec<_>>(),
            [
                (
                    "m1",
                    "chat-1",
                    Role::User,
                    Author::Person,
                    "2026-11-24T13:47:00.000000Z",
                    "Tell me about Berlin"
                ),
                (
                    "m3",
                    "chat-1",
                    Role::Assistant,
                    Author::Unknown,
                    "2026-11-24T13:48:00.000000Z",
                    "Berlin is\n\nthe capital."
                ),
            ]
        );
        let Record::Turn(answer) = &records[1].record else {
            unreachable!()
        };
        assert_eq!(answer.tools, ["web_search"]);
    }

    #[test]
    fn a_cowork_session_gives_its_message_texts_in_its_folder_and_marks_for_tool_steps() {
        // `typed` stands for a message the app sent, which carries its platform.
        let event = |seq: u64, kind: &str, kind_of_payload: &str, typed: bool, content: Value| {
            let mut payload = json!({
                "type": kind_of_payload, "parent_tool_use_id": null,
                "message": { "role": kind_of_payload, "content": content }
            });
            if typed {
                payload["client_platform"] = json!("desktop_app");
            }
            json!({
                "kind": kind, "dedupKey": format!("d{seq}"), "seq": seq,
                "serverCreatedAt": 1_795_531_640_000_u64 + seq * 1000, "payload": payload
            })
        };
        let mut subagent = event(
            9,
            "message",
            "assistant",
            false,
            json!("a subagent's answer"),
        );
        subagent["payload"]["parent_tool_use_id"] = json!("toolu_1");
        let session = json!({
            "product": "cowork",
            "tree": { "events": [
                event(1, "message", "system", false, json!(null)),
                event(2, "message", "user", true, json!("Make the file")),
                event(3, "message", "user", true, json!("<system-reminder>harness</system-reminder>")),
                event(4, "message", "assistant", false, json!([{ "type": "thinking", "thinking": "plan" }])),
                event(5, "message", "assistant", false, json!([
                    { "type": "tool_use", "name": "Bash", "input": { "command": "ls -la" } }
                ])),
                event(6, "message", "user", false, json!([{ "type": "tool_result", "content": "a.txt" }])),
                event(7, "message", "assistant", false, json!([
                    { "type": "text", "text": "<system-reminder> is the harness's tag." }
                ])),
                event(8, "permission_request", "user", true, json!("not a message")),
                subagent,
                event(10, "message", "user", false, json!([{ "type": "text", "text": "Go on" }])),
            ] }
        });
        let folders = BTreeMap::from([("01Ab".to_owned(), "/Users/p/Downloads".to_owned())]);
        let records = records_of(&stored("cowork:cse_01Ab", session), &folders).unwrap();
        assert_eq!(
            records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["d2", "d5", "d7", "d10"]
        );
        assert_eq!(
            turn(&records[0]),
            (
                "d2",
                "cse_01Ab",
                Role::User,
                Author::Person,
                "2026-11-24T14:47:22Z",
                "Make the file"
            )
        );
        let Record::Mark(step) = &records[1].record else {
            panic!("not a mark")
        };
        assert_eq!(step.tools, ["bash:ls"]);
        assert_eq!(step.folder, "/Users/p/Downloads");
        // The assistant's own text is kept, whatever it starts with.
        assert_eq!(
            turn(&records[2]).5,
            "<system-reminder> is the harness's tag."
        );
        assert_eq!(turn(&records[3]).3, Author::Unknown);
    }

    #[test]
    fn a_value_that_cannot_be_read_or_is_no_conversation_is_an_error() {
        let unknown = stored("x", json!({ "product": "notes" }));
        assert!(records_of(&unknown, &BTreeMap::new()).is_err());
        let no_events = stored("cowork:cse_1", json!({ "product": "cowork", "tree": {} }));
        assert!(records_of(&no_events, &BTreeMap::new()).is_err());
        let unreadable = StoredValue {
            key: "k".into(),
            value: Err(crate::ValueError::Format("bad".into())),
        };
        assert_eq!(
            records_of(&unreadable, &BTreeMap::new()),
            Err("bad".to_owned())
        );
    }

    #[test]
    fn the_folders_of_cowork_sessions_come_from_the_apps_session_spaces() {
        let home = std::env::temp_dir().join(format!("claude-app-spaces-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let org = home.join("Library/Application Support/Claude/local-agent-mode-sessions/a/o");
        fs::create_dir_all(&org).unwrap();
        fs::write(
            org.join("remote-session-spaces.json"),
            r#"{"entries":[{"sessionId":"session_01Ab","folders":["/Users/p/Downloads","/x"]}]}"#,
        )
        .unwrap();
        let folders = claude_app_folders(&home).unwrap();
        let _ = fs::remove_dir_all(&home);
        assert_eq!(
            folders,
            BTreeMap::from([("01Ab".to_owned(), "/Users/p/Downloads".to_owned())])
        );
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap().flatten() {
            if entry.path().is_dir() {
                copy_dir(&entry.path(), &to.join(entry.file_name()));
            } else {
                fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
            }
        }
    }

    fn sessions(read: Option<Vec<Conversation>>) -> Option<Vec<String>> {
        read.map(|all| all.into_iter().map(|c| c.session).collect())
    }

    /// A home and a folder for the copy under a fresh `base`, with no Claude app yet.
    struct AppHome {
        base: PathBuf,
        home: PathBuf,
        scratch: PathBuf,
        spaces: PathBuf,
    }

    impl AppHome {
        fn new(name: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("claude-app-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let home = base.join("home");
            Self {
                scratch: base.join("scratch"),
                spaces: sessions_dir(&home).join("a/o"),
                home,
                base,
            }
        }

        /// Installs the app: the test cache, and the fixture's short session in `folder`.
        fn install(&self, folder: &str) {
            let fixture =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-app/IndexedDB");
            copy_dir(&fixture, &claude_app_cache(&self.home));
            fs::create_dir_all(&self.spaces).unwrap();
            self.name_folder(folder);
        }

        fn name_folder(&self, folder: &str) {
            let entries = format!(
                r#"{{"entries":[{{"sessionId":"session_fixture_short","folders":["{folder}"]}}]}}"#
            );
            fs::write(self.spaces.join(SPACES_FILE), entries).unwrap();
        }
    }

    impl Drop for AppHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn the_app_gives_every_conversation_of_its_cache_and_keeps_no_copy() {
        let at = AppHome::new("snapshot");
        let (home, scratch) = (&at.home, &at.scratch);
        let mut app = ClaudeApp::new(scratch.clone());

        assert!(!app.installed(home));
        assert_eq!(sessions(app.read(home).unwrap()), Some(Vec::new()));
        assert_eq!(sessions(app.read(home).unwrap()), None);

        at.install("/w/one");
        assert!(app.installed(home));
        let all = app.read(home).unwrap().unwrap();
        assert!(!scratch.exists());
        assert_eq!(
            all.iter().map(|c| c.session.as_str()).collect::<Vec<_>>(),
            [
                "c0000000-0000-4000-8000-000000000001",
                "cse_fixture_long",
                "cse_fixture_medium",
                "cse_fixture_short"
            ]
        );
        assert_eq!(
            all[0]
                .records
                .as_ref()
                .unwrap()
                .iter()
                .map(turn)
                .collect::<Vec<_>>(),
            [
                (
                    "m0000000-0000-4000-8000-000000000001",
                    "c0000000-0000-4000-8000-000000000001",
                    Role::User,
                    Author::Person,
                    "2026-11-24T13:47:00.000000Z",
                    "Расскажи мне про Берлин"
                ),
                (
                    "m0000000-0000-4000-8000-000000000002",
                    "c0000000-0000-4000-8000-000000000001",
                    Role::Assistant,
                    Author::Unknown,
                    "2026-11-24T13:47:05.000000Z",
                    "Берлин — столица Германии."
                ),
            ]
        );
        let short = all[3].records.as_ref().unwrap();
        assert_eq!(
            short
                .iter()
                .map(turn)
                .map(|t| (t.0, t.4, t.5))
                .collect::<Vec<_>>(),
            [
                (
                    "d0000001",
                    "2026-11-24T14:47:20.001Z",
                    "Создай файл cowork-test.txt"
                ),
                (
                    "d0000002",
                    "2026-11-24T14:47:20.002Z",
                    "Готово: файл создан."
                )
            ]
        );
        assert_eq!(all[1].records.as_ref().unwrap().len(), 120);
        assert_eq!(all[2].records.as_ref().unwrap().len(), 40);
        let Record::Turn(first) = &short[0].record else {
            panic!("not a turn")
        };
        assert_eq!(first.folder, "/w/one");
        assert_eq!(sessions(app.read(home).unwrap()), None);
    }

    #[test]
    fn the_app_is_read_again_only_when_its_cache_or_its_folders_change() {
        let at = AppHome::new("changes");
        let (home, scratch) = (&at.home, &at.scratch);
        at.install("/w/one");
        let mut app = ClaudeApp::new(scratch.clone());
        assert!(app.read(home).unwrap().is_some());

        app.forget();
        assert!(app.read(home).unwrap().is_some());
        at.name_folder("/w/second");
        let again = app.read(home).unwrap().unwrap();
        let Record::Turn(first) = &again[3].records.as_ref().unwrap()[0].record else {
            panic!("not a turn")
        };
        assert_eq!(first.folder, "/w/second");

        // What a Cowork session writes in its own folder is not a change of the cache.
        let outputs = at.spaces.join("local_1/outputs");
        fs::create_dir_all(&outputs).unwrap();
        fs::write(outputs.join("log.txt"), "a step").unwrap();
        assert_eq!(sessions(app.read(home).unwrap()), None);

        // A list of sessions the app is writing, or of a new shape, stops the read until it is
        // whole, and leaves no copy.
        fs::write(at.spaces.join(SPACES_FILE), "{\"entr").unwrap();
        assert!(app.read(home).unwrap_err().contains(SPACES_FILE));
        assert!(!scratch.exists());

        // The erasure takes a copy a crash left behind.
        fs::create_dir_all(scratch).unwrap();
        app.forget();
        assert!(!scratch.exists());

        // A read that panics leaves no copy either.
        let panicked = std::panic::catch_unwind(|| {
            let _copy = Copy(scratch);
            fs::create_dir_all(scratch).unwrap();
            panic!("a read that fails half way");
        });
        assert!(panicked.is_err());
        assert!(!scratch.exists());
    }

    #[test]
    fn a_conversation_without_the_shape_this_reader_knows_is_an_error() {
        let message = json!({ "sender": "human", "created_at": "2026-11-24T13:47:00Z",
                              "content": [{ "type": "text", "text": "hi" }] });
        let chat = |tree: Value| stored("c", json!({ "product": "chat", "tree": tree }));
        let event = json!({ "kind": "message", "dedupKey": "d1", "serverCreatedAt": 1_795_531_640_000_u64,
                            "payload": { "type": "user", "message": { "content": "hi" } } });
        let cowork = |events: Value| {
            stored(
                "cowork:cse_1",
                json!({ "product": "cowork", "tree": { "events": events } }),
            )
        };
        let without = |value: &Value, pointer: &str| {
            let mut value = value.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            value
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            value
        };
        let tree = json!({ "current_leaf_message_uuid": "m1", "parentByChildUuid": {},
                           "messageByUuid": { "m1": message } });
        assert_eq!(
            records_of(&chat(tree.clone()), &BTreeMap::new())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            records_of(&cowork(json!([event])), &BTreeMap::new())
                .unwrap()
                .len(),
            1
        );
        let mut other_sender = tree.clone();
        other_sender["messageByUuid"]["m1"]["sender"] = json!("user");
        let mut other_leaf = tree.clone();
        other_leaf["current_leaf_message_uuid"] = json!("m9");
        for broken in [
            chat(without(&tree, "/current_leaf_message_uuid")),
            chat(other_leaf),
            chat(other_sender),
            chat(without(&tree, "/messageByUuid/m1/created_at")),
            cowork(json!([without(&event, "/kind")])),
            cowork(json!([without(&event, "/dedupKey")])),
            cowork(json!([without(&event, "/serverCreatedAt")])),
            cowork(json!([without(&event, "/payload/type")])),
        ] {
            assert!(records_of(&broken, &BTreeMap::new()).is_err(), "{broken:?}");
        }
        // A chat not begun yet has no message to show.
        let empty = json!({ "current_leaf_message_uuid": null, "messageByUuid": {} });
        assert_eq!(records_of(&chat(empty), &BTreeMap::new()), Ok(Vec::new()));
    }
}
