//! The follower with a snapshot source of its own, whose conversations a test sets before each
//! pass.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use connectors::{Author, Conversation, Identified, Record, Registry, Role, Snapshot, Turn};
use construct_agent::erase::OwnStore;
use construct_agent::follow::{Erase, Follower, Stats, Switch};
use construct_agent::records::{FileSink, Sink};

const SOURCE: &str = "fake-app";

/// What the fake source gives at its next read.
#[derive(Clone)]
enum Store {
    /// These sessions, each with the texts of its turns, or `None` for one that cannot be read.
    Sessions(Vec<(&'static str, Option<Vec<&'static str>>)>),
    /// The store cannot be read.
    Fails,
}

/// A snapshot source whose store is what the test last set, and which counts its reads. It
/// gives `None` while the store is as it gave it last.
#[derive(Clone)]
struct Fake {
    store: Arc<Mutex<Store>>,
    changed: Arc<AtomicBool>,
    reads: Arc<AtomicUsize>,
    installed: Arc<AtomicBool>,
}

impl Fake {
    fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(Store::Sessions(Vec::new()))),
            changed: Arc::new(AtomicBool::new(true)),
            reads: Arc::new(AtomicUsize::new(0)),
            installed: Arc::new(AtomicBool::new(true)),
        }
    }

    fn set(&self, store: Store) {
        *self.store.lock().unwrap() = store;
        self.changed.store(true, Ordering::SeqCst);
    }

    /// As a new run of the agent: the next read gives the store, changed or not.
    fn forget_store(&self) {
        self.changed.store(true, Ordering::SeqCst);
    }
}

fn turn(session: &str, text: &str) -> Identified {
    Identified {
        id: format!("id-{text}"),
        record: Record::Turn(Turn {
            tool: SOURCE.into(),
            session: session.into(),
            time: "2026-11-24T10:00:00Z".into(),
            folder: String::new(),
            branch: String::new(),
            role: Role::User,
            author: Author::Person,
            text: text.into(),
            tools: Vec::new(),
        }),
    }
}

impl Snapshot for Fake {
    fn name(&self) -> &'static str {
        SOURCE
    }

    fn installed(&self, _home: &Path) -> bool {
        self.installed.load(Ordering::SeqCst)
    }

    fn read(&mut self, _home: &Path) -> Result<Option<Vec<Conversation>>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if !self.changed.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        match self.store.lock().unwrap().clone() {
            Store::Fails => Err("denied".into()),
            Store::Sessions(sessions) => Ok(Some(
                sessions
                    .into_iter()
                    .map(|(session, texts)| Conversation {
                        session: session.into(),
                        records: texts
                            .map(|texts| texts.iter().map(|t| turn(session, t)).collect())
                            .ok_or_else(|| "a bad value".to_owned()),
                    })
                    .collect(),
            )),
        }
    }

    fn forget(&mut self) {
        self.changed.store(true, Ordering::SeqCst);
    }
}

struct Setup {
    root: PathBuf,
    data: PathBuf,
    fake: Fake,
    on: Arc<AtomicBool>,
}

impl Setup {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("follow-snapshot-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("home")).unwrap();
        Self {
            data: root.join("data"),
            root,
            fake: Fake::new(),
            on: Arc::new(AtomicBool::new(true)),
        }
    }

    /// A follower as a fresh agent start makes it, with the source switched by `on`.
    fn follower(&self, stats: &Stats, erase: Option<&Erase>) -> Follower<FileSink> {
        let on = self.on.clone();
        let switch: Switch = Arc::new(move |_| on.load(Ordering::SeqCst));
        let follower = Follower::new(
            Registry::new(Vec::new()),
            &self.root.join("home"),
            FileSink::new(&self.data),
            stats.clone(),
        )
        .unwrap()
        .with_snapshots(vec![Box::new(self.fake.clone())])
        .with_switch(switch);
        match erase {
            Some(erase) => follower.with_erase(erase.clone()),
            None => follower,
        }
    }

    /// How many turns of a session are stored.
    fn stored(&self, session: &str) -> usize {
        self.texts(session).len()
    }

    /// The texts of the stored turns of a session, oldest first.
    fn texts(&self, session: &str) -> Vec<String> {
        FileSink::new(&self.data)
            .turns_before(SOURCE, session, "9999", 1000)
            .unwrap()
            .into_iter()
            .map(|t| t.text)
            .collect()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn status(stats: &Stats) -> serde_json::Value {
    stats.to_json()[SOURCE].clone()
}

#[test]
fn a_record_is_stored_once_however_often_the_store_gives_it_and_after_a_restart() {
    let s = Setup::new("once");
    let stats = Stats::default();
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2"]))]));
    let mut follower = s.follower(&stats, None);
    assert_eq!(status(&stats)["scanning"], true);
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 2);
    assert_eq!(status(&stats)["turns"], 2);
    assert_eq!(status(&stats)["scanning"], false);
    follower.pass().unwrap();
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2", "3"]))]));
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 3);
    drop(follower);

    // A new run reads the whole store again and knows what it has seen.
    let stats = Stats::default();
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2", "3", "4"]))]));
    s.follower(&stats, None).pass().unwrap();
    assert_eq!(s.stored("a"), 4);
    assert_eq!(status(&stats)["turns"], 1);
}

#[test]
fn what_is_said_while_the_source_is_off_is_not_stored_when_it_is_switched_on() {
    let s = Setup::new("off");
    let stats = Stats::default();
    s.on.store(false, Ordering::SeqCst);
    s.fake.set(Store::Sessions(vec![("a", Some(vec!["1"]))]));
    s.follower(&stats, None).pass().unwrap();
    assert_eq!(s.stored("a"), 0);
    assert_eq!(status(&stats)["off"], true);
    // What was seen is saved: a restart does not take it for new.
    let mut follower = s.follower(&stats, None);
    s.on.store(true, Ordering::SeqCst);
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2"]))]));
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 1);
    assert_eq!(status(&stats).get("off"), None);
}

#[test]
fn a_session_that_cannot_be_read_keeps_what_was_seen_of_it_and_the_others_are_read() {
    let s = Setup::new("bad-session");
    let stats = Stats::default();
    s.on.store(false, Ordering::SeqCst);
    s.fake.set(Store::Sessions(vec![("a", Some(vec!["1"]))]));
    let mut follower = s.follower(&stats, None);
    follower.pass().unwrap();
    s.on.store(true, Ordering::SeqCst);
    s.fake
        .set(Store::Sessions(vec![("a", None), ("b", Some(vec!["x"]))]));
    follower.pass().unwrap();
    assert_eq!(status(&stats)["unreadable"], 1);
    assert_eq!(
        status(&stats)["error"],
        "cannot read the session a: a bad value"
    );
    assert_eq!((s.stored("a"), s.stored("b")), (0, 1));
    s.fake.set(Store::Sessions(vec![
        ("a", Some(vec!["1", "2"])),
        ("b", Some(vec!["x"])),
    ]));
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 1);
    assert_eq!(status(&stats).get("unreadable"), None);
    assert_eq!(status(&stats).get("error"), None);
}

#[test]
fn a_window_that_slides_over_a_session_loses_nothing_and_never_stores_what_was_said_while_off() {
    let s = Setup::new("window");
    let stats = Stats::default();
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2", "3"]))]));
    let mut follower = s.follower(&stats, None);
    follower.pass().unwrap();
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["3", "4"]))]));
    follower.pass().unwrap();
    assert_eq!(s.texts("a"), ["1", "2", "3", "4"]);

    s.on.store(false, Ordering::SeqCst);
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["4", "5"]))]));
    follower.pass().unwrap();
    s.on.store(true, Ordering::SeqCst);
    s.fake.set(Store::Sessions(vec![("a", Some(vec!["6"]))]));
    follower.pass().unwrap();
    // The app loads the older events again.
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["4", "5", "6"]))]));
    follower.pass().unwrap();
    assert_eq!(s.texts("a"), ["1", "2", "3", "4", "6"]);
}

#[test]
fn a_session_the_store_drops_and_loads_again_never_stores_what_was_said_while_off() {
    let s = Setup::new("evicted");
    let stats = Stats::default();
    s.fake.set(Store::Sessions(vec![
        ("a", Some(vec!["1"])),
        ("b", Some(vec!["9"])),
    ]));
    let mut follower = s.follower(&stats, None);
    follower.pass().unwrap();
    s.on.store(false, Ordering::SeqCst);
    s.fake.set(Store::Sessions(vec![
        ("a", Some(vec!["1", "2"])),
        ("b", Some(vec!["9"])),
    ]));
    follower.pass().unwrap();
    // The app drops the conversation, and then the whole cache.
    s.fake.set(Store::Sessions(vec![("b", Some(vec!["9"]))]));
    follower.pass().unwrap();
    s.fake.set(Store::Sessions(Vec::new()));
    follower.pass().unwrap();
    drop(follower);

    let mut follower = s.follower(&stats, None);
    s.on.store(true, Ordering::SeqCst);
    s.fake.set(Store::Sessions(vec![
        ("a", Some(vec!["1", "2", "3"])),
        ("b", Some(vec!["9"])),
    ]));
    follower.pass().unwrap();
    assert_eq!(s.texts("a"), ["1", "3"]);
}

#[test]
fn an_app_that_is_not_installed_is_no_error_and_says_so() {
    let s = Setup::new("not-installed");
    let stats = Stats::default();
    s.fake.installed.store(false, Ordering::SeqCst);
    let mut follower = s.follower(&stats, None);
    follower.pass().unwrap();
    assert_eq!(
        status(&stats),
        serde_json::json!({ "turns": 0, "skipped": 0, "scanning": false, "not_installed": true })
    );
    s.fake.installed.store(true, Ordering::SeqCst);
    follower.pass().unwrap();
    assert_eq!(status(&stats).get("not_installed"), None);
}

#[test]
fn a_store_that_cannot_be_read_is_an_error_until_it_can() {
    let s = Setup::new("fails");
    let stats = Stats::default();
    s.fake.set(Store::Fails);
    let mut follower = s.follower(&stats, None);
    follower.pass().unwrap();
    assert_eq!(status(&stats)["error"], "cannot read: denied");
    assert_eq!(status(&stats)["scanning"], true);
    s.fake.set(Store::Sessions(vec![("a", Some(vec!["1"]))]));
    follower.pass().unwrap();
    assert_eq!(status(&stats).get("error"), None);
    assert_eq!(s.stored("a"), 1);
}

#[test]
fn a_sink_that_fails_leaves_the_records_unseen_and_the_next_pass_stores_them() {
    let s = Setup::new("sink");
    let stats = Stats::default();
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2"]))]));
    // `records` is a file, so no record can be written.
    fs::create_dir_all(&s.data).unwrap();
    fs::write(s.data.join("records"), "").unwrap();
    let mut follower = s.follower(&stats, None);
    assert!(follower.pass().is_err());
    assert!(
        status(&stats)["error"]
            .as_str()
            .unwrap()
            .starts_with("cannot write records")
    );
    fs::remove_file(s.data.join("records")).unwrap();
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 2);
}

#[test]
fn after_an_erasure_the_source_is_not_read_while_off_and_is_collected_whole_once_on() {
    let s = Setup::new("erase");
    let stats = Stats::default();
    let erase = Erase::new(&s.data, &[SOURCE]);
    s.fake.set(Store::Sessions(vec![("a", Some(vec!["1"]))]));
    let mut follower = s.follower(&stats, Some(&erase));
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 1);

    // While it is on, the store is read again from nothing, changed or not.
    erase.erase().unwrap();
    assert_eq!(s.stored("a"), 0);
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 1);

    erase.erase().unwrap();
    s.on.store(false, Ordering::SeqCst);
    let reads = s.fake.reads.load(Ordering::SeqCst);
    follower.pass().unwrap();
    assert_eq!(s.fake.reads.load(Ordering::SeqCst), reads);
    assert_eq!(status(&stats)["scanning"], false);
    assert_eq!(s.stored("a"), 0);
    drop(follower);

    // A restart while it waits keeps it waiting, and switching it on collects it from nothing.
    let stats = Stats::default();
    let mut follower = s.follower(&stats, Some(&Erase::new(&s.data, &[SOURCE])));
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 0);
    assert_eq!(status(&stats)["scanning"], false);
    s.on.store(true, Ordering::SeqCst);
    s.fake
        .set(Store::Sessions(vec![("a", Some(vec!["1", "2"]))]));
    follower.pass().unwrap();
    assert_eq!(s.stored("a"), 2);
    assert!(!s.data.join("erase-settling").exists());
}

#[test]
fn a_restart_after_an_erasure_that_stopped_halfway_does_not_take_the_old_ids_for_seen() {
    let s = Setup::new("erase-crash");
    let stats = Stats::default();
    s.fake.set(Store::Sessions(vec![("a", Some(vec!["1"]))]));
    s.follower(&stats, Some(&Erase::new(&s.data, &[SOURCE])))
        .pass()
        .unwrap();
    // The erasure noted itself and removed the records, and stopped before the progress.
    fs::write(s.data.join("erase-settling"), SOURCE).unwrap();
    fs::remove_dir_all(s.data.join("records")).unwrap();
    s.fake.forget_store();
    s.follower(&stats, Some(&Erase::new(&s.data, &[SOURCE])))
        .pass()
        .unwrap();
    assert_eq!(s.texts("a"), ["1"]);
}
