//! `memory-erase` and the agent's own copies: the collected sessions with the saved positions of
//! the readers, the journal of the sent prompts and the chat conversations, on temporary
//! directories, with the real Claude Code and Codex readers over a temporary home and a fake host.
#![cfg(unix)]
#![allow(clippy::unwrap_used)] // helpers outside #[test] functions use it too

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use connectors::{Author, ClaudeCode, Codex, Registry, Role, Turn};
use construct_agent::activity::Feed;
use construct_agent::chat::{Chat, Link};
use construct_agent::dashboard_setup::SetupJob;
use construct_agent::erase::{OwnStore, OwnStores, own_stores};
use construct_agent::follow::{self, Erase, Follower, Stats};
use construct_agent::manage::Manager;
use construct_agent::records::{FileSink, Sink};
use construct_agent::settings::{Collection, Settings, Update};
use construct_agent::workspace::Workspace;
use serde_json::{Value, json};

mod common;

use common::fake_host::FakeHost;
use common::scripted::{Turn as Scripted, provider};
use common::stand::{deadlines, timing, wait_for};
use common::tools::{Under, claude_user, codex_meta, codex_user, recent_claude, recent_codex};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const SETTINGS_ON: &str =
    r#"{"subject_id":"s","personalization_enabled":true,"erasure_in_progress":false}"#;
const ERASING: &str =
    r#"{"subject_id":"s","personalization_enabled":false,"erasure_in_progress":true}"#;
const CLAUDE_LOG: &str = ".claude/projects/project/sess-1.jsonl";
const CODEX_ID: &str = "01a0bee2-6260-73a3-b389-43c86bb8930f";
const CODEX_LOG: &str = ".codex/sessions/2026/11/11/rollout-2026-11-11T10-15-00-01a0bee2-6260-73a3-b389-43c86bb8930f.jsonl";

fn registry() -> Registry {
    Registry::new(vec![
        Box::new(Under(ClaudeCode, ".claude/projects")),
        Box::new(Under(Codex, ".codex/sessions")),
    ])
}

/// The names the erasure owns, as the agent gives them.
fn sources() -> Vec<&'static str> {
    follow::source_names(&registry())
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

fn append(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

/// Everything under `dir` that is a file: its path (relative), its bytes and its change time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>, SystemTime)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                let relative = path.strip_prefix(root).unwrap().to_owned();
                out.push((relative, fs::read(&path).unwrap(), modified));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn route_calls(host: &FakeHost) -> Vec<String> {
    host.calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| format!("{} {}", c.method, c.path.rsplit('/').next().unwrap()))
        .collect()
}

struct Stand {
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    settings: Settings,
    manage: Manager,
    chat: Chat,
    erase: Erase,
    host: FakeHost,
    /// Runs the chat's answers; kept alive with the stand.
    _rt: tokio::runtime::Runtime,
    stats: Stats,
}

impl Stand {
    fn new(provider_url: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "cb-erase-own-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let (home, data) = (root.join("home"), root.join("data"));
        fs::create_dir_all(&home).unwrap();
        let settings = Settings::open(&root.join("config")).unwrap();
        let model = json!({ "model": { "base_url": provider_url, "model": "m", "api_key": "k" } });
        settings
            .apply(Update::parse(&model.to_string()).unwrap())
            .unwrap();
        // A switch that differs from the default, so a reset of the settings would show.
        settings
            .apply(Update::parse(r#"{"collection":{"codex":false}}"#).unwrap())
            .unwrap();
        let link = Link::default();
        let host = FakeHost::start();
        link.set(host.connection());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let manage = Manager::open_with(&data, settings.clone(), link.clone(), timing()).unwrap();
        let chat = Chat::new(settings.clone(), &data, rt.handle().clone(), deadlines())
            .unwrap()
            .with_link(link);
        let erase = Erase::new(&data, &sources());
        manage.set_own_stores(own_stores(
            &erase,
            Some(&chat),
            &manage,
            &Feed::new(),
            None,
            None,
            rt.handle(),
        ));
        // Codex's own switch is on again for the tests that read both tools.
        settings
            .apply(Update::parse(r#"{"collection":{"codex":true}}"#).unwrap())
            .unwrap();
        Self {
            root,
            home,
            data,
            settings,
            manage,
            chat,
            erase,
            host,
            _rt: rt,
            stats: Stats::default(),
        }
    }

    fn follower(&self) -> Follower<FileSink> {
        self.follower_with(self.erase.clone())
    }

    fn follower_with(&self, erase: Erase) -> Follower<FileSink> {
        let settings = self.settings.clone();
        Follower::new(
            registry(),
            &self.home,
            FileSink::new(&self.data),
            self.stats.clone(),
        )
        .unwrap()
        .with_snapshots(follow::snapshots(&self.data))
        .with_switch(Arc::new(move |tool| settings.collects(tool)))
        .with_erase(erase)
    }

    /// A follower in a new process: its `Erase` handle knows of an erasure only what is on the
    /// disk.
    fn restarted(&self) -> Follower<FileSink> {
        self.follower_with(Erase::new(&self.data, &sources()))
    }

    /// The Claude app has its cache (the test one of the reader).
    fn fill_app(&self, follower: &mut Follower<FileSink>) {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../connectors/tests/fixtures/claude-app/IndexedDB");
        copy_dir(&fixture, &connectors::claude_app_cache(&self.home));
        follower.pass().unwrap();
    }

    /// Whether the saved position of a file is at its end.
    fn read_to_the_end(&self, path: &Path) -> bool {
        let length = fs::metadata(path).unwrap().len();
        FileSink::new(&self.data)
            .load_progress()
            .unwrap()
            .files
            .iter()
            .any(|(file, progress)| Path::new(file) == path && progress.offset == length)
    }

    fn claude(&self) -> PathBuf {
        self.home.join(CLAUDE_LOG)
    }

    fn codex(&self) -> PathBuf {
        self.home.join(CODEX_LOG)
    }

    /// The two tools have a session each, the chat has a conversation, and the journal has a line.
    fn fill(&self, follower: &mut Follower<FileSink>, provider: bool) {
        append(&self.claude(), &format!("{}\n", claude_user("claude old")));
        append(
            &self.codex(),
            &format!("{}\n{}\n", codex_meta(CODEX_ID), codex_user("codex old")),
        );
        follower.pass().unwrap();
        if provider {
            let mut out = Vec::new();
            self.chat.send(
                &json!({ "message": "a chat question" }).to_string(),
                &mut |line| {
                    out.push(line.to_owned());
                    true
                },
            );
            assert!(out.last().unwrap().contains("done"), "{out:?}");
        }
        // A live prompt of a tool that is switched off is refused, and the journal notes it.
        self.settings
            .apply(Update::parse(r#"{"collection":{"codex":false}}"#).unwrap())
            .unwrap();
        (self.manage.observer())(&Turn {
            tool: "codex".into(),
            session: "s".into(),
            time: "2999-01-01T00:00:00Z".into(),
            folder: String::new(),
            branch: String::new(),
            role: Role::User,
            author: Author::Person,
            text: "typed while the tool was off".into(),
            tools: Vec::new(),
        });
        self.settings
            .apply(Update::parse(r#"{"collection":{"codex":true}}"#).unwrap())
            .unwrap();
    }

    fn records(&self) -> Vec<PathBuf> {
        FileSink::new(&self.data).session_files().unwrap()
    }

    fn journal(&self) -> Vec<u8> {
        fs::read(self.data.join("manage-sent.log")).unwrap_or_default()
    }

    fn progress_file(&self) -> PathBuf {
        self.data.join("connectors-progress.json")
    }

    fn erase_command(&self) -> Value {
        self.host.answer(200, SETTINGS_ON);
        self.host.answer(202, ERASING);
        serde_json::from_str(&self.manage.erase()).unwrap()
    }

    fn switch_on(&self) {
        self.host.answer(200, SETTINGS_ON);
        self.manage
            .apply_settings(Update::parse(r#"{"collection":{"enabled":true}}"#).unwrap())
            .unwrap();
    }

    /// The stored texts of one session, in alphabetical order: the made-up lines share a time.
    fn stored_texts(&self, tool: &str, session: &str) -> Vec<String> {
        let mut texts = FileSink::new(&self.data)
            .turns_before(tool, session, "9999", 1000)
            .unwrap()
            .into_iter()
            .map(|t| t.text)
            .collect::<Vec<_>>();
        texts.sort();
        texts
    }
}

impl Drop for Stand {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn the_erase_command_leaves_no_record_no_position_no_journal_and_no_conversation() {
    let (addr, _) = provider(vec![Scripted::Text("the answer")]);
    let stand = Stand::new(&format!("http://{addr}/v1"));
    let mut follower = stand.follower();
    stand.fill(&mut follower, true);
    let sources: Vec<String> = stand
        .records()
        .iter()
        .map(|p| {
            p.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into()
        })
        .collect();
    assert_eq!(sources.len(), 3, "{sources:?}");
    assert!(stand.progress_file().exists());
    assert!(!stand.journal().is_empty());
    assert!(stand.chat.list().contains("a chat question"));
    // What the chat asked the host for while it answered is not the erasure's.
    stand.host.calls.lock().unwrap().clear();
    let tools_before = snapshot(&stand.home);
    let model = stand.settings.model();
    let before = stand.settings.collection();

    let answer = stand.erase_command();

    assert_eq!(answer["erasure"], "started", "{answer}");
    assert!(stand.records().is_empty());
    assert!(!stand.progress_file().exists());
    assert!(stand.journal().is_empty());
    assert_eq!(
        stand.chat.list(),
        json!({ "conversations": [] }).to_string()
    );
    // The files of the tools are as they were, down to the change time.
    assert_eq!(snapshot(&stand.home), tools_before);
    // The model and the switches stay; collection is off.
    assert_eq!(stand.settings.model(), model);
    assert_eq!(
        stand.settings.collection(),
        Collection {
            enabled: false,
            ..before
        }
    );
    assert_eq!(route_calls(&stand.host), ["GET settings", "POST erasure"]);
}

#[test]
fn the_erase_command_takes_the_claude_app_sessions_and_a_switch_on_collects_them_again() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill_app(&mut follower);
    let short = ["Готово: файл создан.", "Создай файл cowork-test.txt"];
    assert_eq!(stand.stored_texts("claude-app", "cse_fixture_short"), short);
    let progress = FileSink::new(&stand.data).load_progress().unwrap();
    assert!(progress.seen.contains_key("claude-app"));

    stand.erase_command();
    assert!(stand.records().is_empty());
    assert!(!stand.progress_file().exists());
    // While collection is off after the erasure, the app is not read again.
    follower.pass().unwrap();
    assert!(stand.records().is_empty());

    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-app", "cse_fixture_short"), short);
}

#[test]
fn after_a_switch_on_the_old_session_files_are_collected_again_and_a_new_line_once() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    // The readers go on while collection is off: they read nothing and set no position.
    follower.pass().unwrap();
    assert!(stand.records().is_empty());
    assert!(!stand.progress_file().exists());

    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert_eq!(stand.stored_texts("codex", CODEX_ID), ["codex old"]);

    append(&stand.claude(), &format!("{}\n", claude_user("claude new")));
    append(&stand.codex(), &format!("{}\n", codex_user("codex new")));
    follower.pass().unwrap();
    follower.pass().unwrap();
    assert_eq!(
        stand.stored_texts("claude-code", "sess-1"),
        ["claude new", "claude old"]
    );
    assert_eq!(
        stand.stored_texts("codex", CODEX_ID),
        ["codex new", "codex old"]
    );
    // Each line was read once: the saved positions are at the ends of the files.
    assert!(stand.read_to_the_end(&stand.claude()));
    assert!(stand.read_to_the_end(&stand.codex()));
}

#[test]
fn a_switch_on_at_once_after_the_erasure_collects_the_old_session_files_too() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    // No pass has run while collection was off.
    stand.switch_on();

    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert!(
        stand.progress_file().exists(),
        "the readers did not note where they are"
    );
}

#[test]
fn a_line_written_while_collection_is_off_after_the_erasure_is_stored_once() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    append(&stand.claude(), &format!("{}\n", claude_user("claude new")));
    follower.pass().unwrap();
    assert!(stand.records().is_empty());

    // A line that does not parse is counted each time it is read, whatever the sink drops.
    append(&stand.claude(), "not a log line\n");
    stand.switch_on();
    follower.pass().unwrap();
    follower.pass().unwrap();
    assert_eq!(
        stand.stored_texts("claude-code", "sess-1"),
        ["claude new", "claude old"]
    );
    assert_eq!(stand.stats.to_json()["claude-code"]["skipped"], 1);
    assert!(stand.read_to_the_end(&stand.claude()));
}

#[test]
fn after_the_erasure_and_a_restart_no_position_is_left_while_collection_is_off() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    drop(follower);

    let mut restarted = stand.restarted();
    restarted.pass().unwrap();
    restarted.pass().unwrap();
    assert!(stand.records().is_empty());
    assert!(!stand.progress_file().exists());
    // The marker stays: the erasure is not settled until a tool is read while it is on.
    assert!(stand.data.join("erase-settling").exists());

    stand.switch_on();
    restarted.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert!(stand.progress_file().exists());
    assert!(!stand.data.join("erase-settling").exists());
}

#[test]
fn a_tool_that_stays_off_after_the_erasure_keeps_no_position_and_the_marker_names_it() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    // Claude is switched on, Codex stays off by its own switch.
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"codex":false}}"#).unwrap())
        .unwrap();
    stand.switch_on();
    follower.pass().unwrap();

    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert!(stand.stored_texts("codex", CODEX_ID).is_empty());
    let positions = fs::read_to_string(stand.progress_file()).unwrap();
    assert!(positions.contains("sess-1.jsonl"), "{positions}");
    assert!(!positions.contains("rollout-"), "{positions}");
    assert_eq!(
        fs::read_to_string(stand.data.join("erase-settling")).unwrap(),
        "codex"
    );

    // Codex is switched on: it is collected from the start and the marker goes.
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"codex":true}}"#).unwrap())
        .unwrap();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("codex", CODEX_ID), ["codex old"]);
    assert!(!stand.data.join("erase-settling").exists());
}

#[test]
fn a_tool_read_while_on_after_the_erasure_is_not_held_back_by_a_restart_while_another_waits() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"codex":false}}"#).unwrap())
        .unwrap();
    stand.switch_on();
    follower.pass().unwrap();
    drop(follower);

    // Collection goes off and the agent restarts; Codex still waits, Claude does not.
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"enabled":false}}"#).unwrap())
        .unwrap();
    append(&stand.claude(), &format!("{}\n", claude_user("while off")));
    let mut restarted = stand.restarted();
    restarted.pass().unwrap();
    stand.switch_on();
    restarted.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
}

#[test]
fn positions_a_crash_in_the_middle_of_the_erasure_left_are_dropped_at_the_restart() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    let positions = fs::read(stand.progress_file()).unwrap();
    stand.erase_command();
    drop(follower);
    // The records are gone, the marker is there, and the positions were not removed yet.
    fs::write(stand.progress_file(), positions).unwrap();
    fs::write(stand.data.join("erase-settling"), "claude-code\ncodex").unwrap();

    let mut restarted = stand.restarted();
    restarted.pass().unwrap();
    assert!(stand.records().is_empty());
    assert!(
        !fs::read_to_string(stand.progress_file())
            .unwrap_or_default()
            .contains("sess-1.jsonl")
    );
    stand.switch_on();
    restarted.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert_eq!(stand.stored_texts("codex", CODEX_ID), ["codex old"]);
}

#[test]
fn a_file_that_can_only_be_read_while_off_is_dropped_as_under_the_plain_rule() {
    use std::os::unix::fs::PermissionsExt;
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    let locked = stand.home.join(".claude/projects/project/locked.jsonl");
    append(&locked, &format!("{}\n", claude_user("locked")));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    stand.erase_command();
    stand.switch_on();
    follower.pass().unwrap();
    // The tool was read while on: it no longer waits.
    assert!(!stand.data.join("erase-settling").exists());

    // The tool is switched off, and only then can the file be read: it is read and dropped.
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"enabled":false}}"#).unwrap())
        .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    follower.pass().unwrap();
    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
}

#[test]
fn a_file_that_stays_unreadable_does_not_change_what_a_plain_switch_off_drops() {
    use std::os::unix::fs::PermissionsExt;
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    let locked = stand.home.join(".claude/projects/project/locked.jsonl");
    append(&locked, &format!("{}\n", claude_user("locked")));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    stand.erase_command();
    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);

    // Collection goes off and the tool writes meanwhile: the readable files are read and dropped.
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"enabled":false}}"#).unwrap())
        .unwrap();
    append(&stand.claude(), &format!("{}\n", claude_user("while off")));
    follower.pass().unwrap();
    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
}

#[test]
fn a_marker_that_cannot_be_updated_is_shown_in_status_and_tried_again() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    // The marker cannot be removed: a directory stands in its place.
    let marker = stand.data.join("erase-settling");
    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    stand.switch_on();
    follower.pass().unwrap();
    let status = stand.stats.to_json()["claude-code"].clone();
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .contains("cannot update the erasure note"),
        "{status}"
    );

    // Still blocked: the next pass tries again and the error is still shown.
    follower.pass().unwrap();
    assert!(
        stand.stats.to_json()["claude-code"]["error"]
            .as_str()
            .unwrap()
            .contains("cannot update the erasure note")
    );

    // Once the blocker is gone the same marker is removed by the retry, not by the test.
    fs::remove_dir(&marker).unwrap();
    fs::write(&marker, "claude-code\ncodex").unwrap();
    follower.pass().unwrap();
    assert!(stand.stats.to_json()["claude-code"].get("error").is_none());
    assert!(!marker.exists());
}

#[test]
fn a_plain_switch_off_and_on_still_drops_what_the_tools_wrote_meanwhile() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"enabled":false}}"#).unwrap())
        .unwrap();
    append(&stand.claude(), &format!("{}\n", claude_user("while off")));
    follower.pass().unwrap();
    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
}

#[test]
fn running_chat_answers_are_all_stopped_and_waited_for_before_the_conversations_go() {
    // The provider never answers: the answers run until they are stopped.
    let (addr, bodies) = provider(vec![Scripted::Silence, Scripted::Silence]);
    let stand = Stand::new(&format!("http://{addr}/v1"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let asking: Vec<_> = [
        "a question that gets no answer",
        "another question without answer",
    ]
    .into_iter()
    .map(|message| {
        let (chat, events) = (stand.chat.clone(), Arc::clone(&events));
        thread::spawn(move || {
            chat.send(&json!({ "message": message }).to_string(), &mut |line| {
                events.lock().unwrap().push(line.to_owned());
                true
            });
        })
    })
    .collect();
    wait_for("the requests to the provider", || {
        bodies.lock().unwrap().len() == 2
    });
    assert!(stand.chat.list().contains("a question that gets no answer"));
    assert!(
        stand
            .chat
            .list()
            .contains("another question without answer")
    );

    OwnStore::erase(&stand.chat).unwrap();

    // Both answers had ended when the erasure returned, and they left nothing behind.
    for asking in asking {
        wait_for("the answer to end", || asking.is_finished());
        asking.join().unwrap();
    }
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .filter(|line| line.contains(r#""stopped":true"#))
            .count(),
        2,
        "{events:?}"
    );
    assert_eq!(
        stand.chat.list(),
        json!({ "conversations": [] }).to_string()
    );
    assert!(!stand.data.join("records/chat").exists());
}

struct Failing;

impl OwnStore for Failing {
    fn name(&self) -> &'static str {
        "a store that fails"
    }

    fn erase(&self) -> Result<(), String> {
        Err("no".into())
    }
}

#[test]
fn a_store_that_fails_is_named_and_the_others_are_still_erased() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    let list = OwnStores::new(vec![
        Arc::new(Failing),
        Arc::new(stand.erase.clone()),
        stand.manage.journal_store(),
    ]);

    assert_eq!(list.erase(), ["a store that fails"]);
    assert!(stand.records().is_empty());
    assert!(stand.journal().is_empty());
}

#[test]
fn the_erase_command_tells_when_the_agent_could_not_erase_a_copy_and_still_asks_the_host() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let manage = Manager::open_with(
        &stand.root.join("other"),
        stand.settings.clone(),
        {
            let link = Link::default();
            link.set(stand.host.connection());
            link
        },
        timing(),
    )
    .unwrap();
    manage.set_own_stores(OwnStores::new(vec![Arc::new(Failing)]));
    stand.host.answer(200, SETTINGS_ON);
    stand.host.answer(202, ERASING);

    let answer: Value = serde_json::from_str(&manage.erase()).unwrap();

    let error = answer["error"].as_str().unwrap();
    assert!(error.contains("a store that fails"), "{answer}");
    assert!(error.contains("the erasure started"), "{answer}");
    assert!(!stand.settings.collection().enabled);
    assert_eq!(route_calls(&stand.host), ["GET settings", "POST erasure"]);
}

#[test]
fn a_record_stored_by_a_follower_that_ran_before_is_not_dropped_as_a_replay_afterwards() {
    // The sink remembers what it stored; with the files gone that memory must go too, or the
    // same line written again would be taken for a replay and lost.
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    stand.erase_command();
    follower.pass().unwrap();
    stand.switch_on();
    follower.pass().unwrap();

    // The same file content again, as a new file with the same line.
    let again = stand.home.join(".claude/projects/project/sess-2.jsonl");
    append(&again, &format!("{}\n", claude_user("claude old")));
    follower.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
}

#[test]
fn an_old_file_is_left_alone_after_the_erasure_until_it_is_written_to() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    // A session file nobody touched for 31 days, which no pass has read.
    append(
        &stand.claude(),
        &format!("{}\n{}\n", claude_user("old one"), claude_user("old two")),
    );
    let long_ago = SystemTime::now() - Duration::from_hours(31 * 24);
    fs::File::options()
        .write(true)
        .open(stand.claude())
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    follower.pass().unwrap();
    assert!(stand.records().is_empty());

    stand.erase_command();
    follower.pass().unwrap();
    stand.switch_on();
    follower.pass().unwrap();
    assert!(
        stand.records().is_empty(),
        "a file older than 30 days was collected"
    );

    // The session is resumed: its change time is new now, and it is read as a new file.
    append(&stand.claude(), &format!("{}\n", claude_user("new line")));
    follower.pass().unwrap();
    assert_eq!(
        stand.stored_texts("claude-code", "sess-1"),
        ["new line", "old one", "old two"]
    );
}

#[test]
fn an_agent_restarted_before_the_first_pass_after_the_erasure_collects_the_last_30_days_only() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    // An old file no pass has read, and an erasure; the agent stops before any pass.
    let old = stand.home.join(".claude/projects/project/sess-old.jsonl");
    append(&old, &format!("{}\n", claude_user("old history")));
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_hours(31 * 24))
        .unwrap();
    stand.erase_command();
    drop(follower);

    // A new process knows nothing of the erasure but what is on the disk.
    let mut restarted = stand.restarted();
    stand.switch_on();
    restarted.pass().unwrap();
    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert_eq!(stand.stored_texts("codex", CODEX_ID), ["codex old"]);
    assert!(!stand.data.join("erase-settling").exists());
}

#[test]
fn a_file_that_cannot_be_read_does_not_hold_the_others_and_is_collected_once_it_can_be() {
    use std::os::unix::fs::PermissionsExt;
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    let locked = stand.home.join(".claude/projects/project/locked.jsonl");
    append(&locked, &format!("{}\n", claude_user("locked")));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    stand.erase_command();
    stand.switch_on();
    follower.pass().unwrap();

    assert_eq!(stand.stored_texts("claude-code", "sess-1"), ["claude old"]);
    assert_eq!(stand.stats.to_json()["claude-code"]["unreadable"], 1);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
    follower.pass().unwrap();
    assert_eq!(
        stand.stored_texts("claude-code", "sess-1"),
        ["claude old", "locked"]
    );
}

#[test]
fn the_follower_erase_leaves_a_source_it_does_not_follow() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    stand.fill(&mut follower, false);
    let foreign = stand.data.join("records/other/x.jsonl");
    append(&foreign, "{}\n");

    OwnStore::erase(&stand.erase).unwrap();

    assert!(foreign.exists());
    assert!(!stand.data.join("records/claude-code").exists());
    assert!(!stand.data.join("records/codex").exists());
}

#[test]
fn a_dashboard_setup_run_reads_every_recent_session_again_once_collection_is_switched_on() {
    let stand = Stand::new("http://127.0.0.1:9/v1");
    let mut follower = stand.follower();
    for n in 1..=3_i64 {
        append(
            &stand
                .home
                .join(format!(".claude/projects/project/sess-{n}.jsonl")),
            &format!(
                "{}\n",
                recent_claude(&format!("sess-{n}"), "claude work", n)
            ),
        );
        let id = format!("01a0bee2-6260-73a3-b389-43c86bb8930{n}");
        append(
            &stand.home.join(format!(
                ".codex/sessions/2026/11/11/rollout-2026-11-11T10-15-00-{id}.jsonl"
            )),
            &format!("{}\n{}\n", codex_meta(&id), recent_codex("codex work", n)),
        );
    }
    follower.pass().unwrap();
    assert_eq!(stand.records().len(), 6);
    let workspace = Arc::new(Workspace::open(&stand.data).unwrap());
    let setup = SetupJob::open(
        stand.settings.clone(),
        stand.chat.clone(),
        workspace,
        &stand.data,
    );
    let sessions_read = |setup: &SetupJob| -> Value {
        let _ = setup.start(r#"{"days":30}"#);
        let mut status = Value::Null;
        wait_for("the run to end", || {
            status = serde_json::from_str(&setup.status_line()).unwrap();
            status["state"] == "failed"
        });
        status["sessions"].clone()
    };

    stand.erase_command();
    follower.pass().unwrap();
    // Collection is off: the run is refused, and nothing was stored or noted.
    assert!(setup.start(r#"{"days":30}"#).contains("error"));
    assert!(stand.records().is_empty());
    assert!(!stand.progress_file().exists());

    stand.switch_on();
    follower.pass().unwrap();
    assert_eq!(stand.records().len(), 6);
    // The model is not reachable, so the run fails after it has written the session files:
    // all six sessions were there to read.
    assert_eq!(sessions_read(&setup), 6);
}
