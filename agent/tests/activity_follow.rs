//! The activity store kept up to date from the follower, on a fake history. These tests unpack
//! and start PostgreSQL, so they are ignored by default: `cargo test -- --ignored`.
//!
//! The fake tool reads lines of `<time> <author> <folder> <branch>`, each one a user prompt.

#![cfg(target_os = "linux")]
// Test code: a failed `unwrap` is a failed test.
#![allow(clippy::unwrap_used)]

mod common;

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::pg_stand::{PgStand, append};
use connectors::{Author, FileState, Listing, Parsed, Record, Registry, Role, Tool, Turn};
use construct_agent::activity::{Feed, Session, Store};
use construct_agent::chat::Link;
use construct_agent::erase::own_stores;
use construct_agent::follow::{self, Erase, Follower, Stats};
use construct_agent::ipc::{Server, query_status};
use construct_agent::manage::Manager;
use construct_agent::records::FileSink;
use construct_agent::settings::Settings;

struct Fake;

impl Tool for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn root(&self, home: &Path) -> PathBuf {
        home.join(".fake")
    }

    fn find_files(&self, root: &Path) -> io::Result<Listing> {
        let mut listing = Listing::default();
        if let Ok(dir) = fs::read_dir(root) {
            for entry in dir {
                listing.files.push(entry?.path());
            }
        }
        listing.files.sort();
        Ok(listing)
    }

    fn open_file(&self, path: &Path) -> FileState {
        FileState {
            session: path.file_stem().unwrap().to_string_lossy().into_owned(),
            ..FileState::default()
        }
    }

    fn parse_line(&self, state: &mut FileState, line: &str) -> Parsed {
        let mut words = line.split(' ');
        let (Some(time), Some(author), Some(folder), Some(branch)) =
            (words.next(), words.next(), words.next(), words.next())
        else {
            return Parsed::Skipped;
        };
        let author = match author {
            "person" => Author::Person,
            "unattended" => Author::Unattended,
            _ => Author::Unknown,
        };
        Parsed::Record(Record::Turn(Turn {
            tools: Vec::new(),
            tool: "fake".into(),
            session: state.session.clone(),
            time: time.into(),
            folder: folder.into(),
            branch: branch.into(),
            role: Role::User,
            author,
            text: line.into(),
        }))
    }
}

/// The database stand, with a home for the fake tool's logs and a data directory for records.
struct Stand(PgStand);

impl std::ops::Deref for Stand {
    type Target = PgStand;

    fn deref(&self) -> &PgStand {
        &self.0
    }
}

impl Stand {
    async fn new(name: &str) -> Self {
        let stand = Self(PgStand::new(name).await);
        fs::create_dir_all(stand.home().join(".fake")).unwrap();
        stand
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn data(&self) -> PathBuf {
        self.root.join("data/records")
    }

    fn log(&self, session: &str) -> PathBuf {
        self.home().join(".fake").join(format!("{session}.log"))
    }

    /// A follower of the fake tool whose new records also go to `feed`.
    fn follower(&self, feed: &Feed) -> Follower<FileSink> {
        Follower::new(
            Registry::new(vec![Box::new(Fake)]),
            &self.home(),
            FileSink::new(&self.data()).with_recorder(feed.recorder()),
            Stats::default(),
        )
        .unwrap()
    }

    /// Runs the feed on `store`, building from the records this stand keeps.
    fn run(&self, feed: &Feed, store: &Arc<Store>) -> tokio::task::JoinHandle<()> {
        let (feed, store, data) = (feed.clone(), store.clone(), self.data());
        tokio::spawn(async move { feed.run(&store, &FileSink::new(&data)).await })
    }

    async fn finish(self, store: &Store, task: tokio::task::JoinHandle<()>) {
        task.abort();
        let _ = task.await;
        store.close().await;
        self.0.finish().await;
    }
}

/// The feed's state as `status` shows it.
fn phase(feed: &Feed) -> String {
    feed.to_json()["state"].as_str().unwrap().to_owned()
}

/// Waits until `done` accepts the sessions of `store`, and says how long it took.
async fn wait_for(store: &Store, limit: Duration, done: impl Fn(&[Session]) -> bool) -> Duration {
    let start = Instant::now();
    loop {
        if done(&store.sessions().await.unwrap()) {
            return start.elapsed();
        }
        assert!(start.elapsed() < limit, "the store did not follow in time");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for the feed to be in `wanted` (with no error shown when `clean`).
async fn wait_phase(feed: &Feed, wanted: &str, clean: bool, limit: Duration) {
    let start = Instant::now();
    while phase(feed) != wanted || (clean && feed.to_json().get("error").is_some()) {
        assert!(
            start.elapsed() < limit,
            "the feed was {}, not {wanted}",
            feed.to_json()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn session<'a>(sessions: &'a [Session], id: &str) -> &'a Session {
    sessions.iter().find(|s| s.session == id).unwrap()
}

/// The store as the tests compare it.
async fn contents(store: &Store) -> (Vec<Session>, Vec<(String, String, String)>) {
    (
        store.sessions().await.unwrap(),
        store.minutes().await.unwrap(),
    )
}

/// `sessions` sessions of `prompts` prompts each, a minute and a half apart, the authors in turn
/// person, unattended, unknown; session `n` is in folder `/work/<n % 7>`.
fn write_history(stand: &Stand, sessions: usize, prompts: usize) {
    for n in 0..sessions {
        let mut lines = String::new();
        for i in 0..prompts {
            let at = i * 90;
            let author = ["person", "unattended", "unknown"][i % 3];
            writeln!(
                lines,
                "2026-11-{:02}T{:02}:{:02}:{:02}Z {author} /work/{} main",
                1 + n % 28,
                at / 3600,
                at / 60 % 60,
                at % 60,
                n % 7,
            )
            .unwrap();
        }
        append(&stand.log(&format!("s{n:03}")), &lines);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn a_record_appended_to_a_log_shows_in_the_store_within_ten_seconds() {
    let stand = Stand::new("follow-live").await;
    let feed = Feed::new();
    let store = Arc::new(stand.store().await);
    let task = stand.run(&feed, &store);
    wait_phase(&feed, "ready", true, Duration::from_secs(60)).await;

    // The real follower, on its real timer: a pass every 5 s leaves room for the write.
    assert!(follow::POLL_INTERVAL <= Duration::from_secs(5));
    let alive = follow::spawn_observed(
        Registry::new(vec![Box::new(Fake)]),
        Vec::new(),
        Ok((stand.home(), stand.data())),
        &Stats::default(),
        None,
        None,
        Some(feed.recorder()),
        None,
    );
    append(
        &stand.log("s1"),
        "2026-11-11T10:00:05Z person /work/x main\n",
    );
    let took = wait_for(&store, Duration::from_secs(10), |s| {
        s.iter().any(|s| s.session == "s1" && s.turns == 1)
    })
    .await;
    eprintln!("first record after {took:?}");

    append(
        &stand.log("s1"),
        "2026-11-11T10:07:30Z person /work/x main\n",
    );
    wait_for(&store, Duration::from_secs(10), |s| {
        s.iter()
            .any(|s| s.session == "s1" && s.turns == 2 && s.active_minutes == 2)
    })
    .await;

    drop(alive);
    stand.finish(&store, task).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn a_late_person_prompt_changes_the_author_counts_of_its_session_only() {
    let stand = Stand::new("follow-author").await;
    append(
        &stand.log("a"),
        "2026-11-11T10:00:00Z unknown /work/x main\n2026-11-11T10:01:00Z unattended /work/x main\n",
    );
    append(
        &stand.log("b"),
        "2026-11-11T11:00:00Z person /work/y main\n",
    );
    let feed = Feed::new();
    let store = Arc::new(stand.store().await);
    let task = stand.run(&feed, &store);
    let mut follower = stand.follower(&feed);
    follower.pass().unwrap();
    wait_for(&store, Duration::from_secs(20), |s| {
        s.len() == 2 && session(s, "a").turns == 2
    })
    .await;
    let before = store.sessions().await.unwrap();
    let a = session(&before, "a");
    assert_eq!(
        (a.person_prompts, a.unattended_prompts, a.unknown_prompts),
        (0, 1, 1)
    );
    assert_eq!(a.active_minutes, 2);

    // The person speaks late, from another folder.
    append(
        &stand.log("a"),
        "2026-11-11T10:20:00Z person /work/z feature\n",
    );
    follower.pass().unwrap();
    wait_for(&store, Duration::from_secs(20), |s| {
        session(s, "a").person_prompts == 1
    })
    .await;
    let after = store.sessions().await.unwrap();
    let a = session(&after, "a");
    assert_eq!(
        (a.person_prompts, a.unattended_prompts, a.unknown_prompts),
        (1, 1, 1)
    );
    assert_eq!(a.turns, 3);
    assert_eq!(a.active_minutes, 3);
    assert_eq!(
        (a.folder.as_str(), a.branch.as_str()),
        ("/work/z", "feature")
    );
    assert_eq!(a.last_time, "2026-11-11T10:20:00Z");
    // Nothing else moved.
    assert_eq!(session(&after, "b"), session(&before, "b"));
    let minutes = store.minutes().await.unwrap();
    assert_eq!(minutes.iter().filter(|m| m.1 == "b").count(), 1);

    stand.finish(&store, task).await;
}

/// Asks `status` over and over until the activity store is ready. Every reply must say the agent
/// runs, the count of records read never goes back, and some reply shows it above nothing.
async fn status_until_ready(socket: &Path) {
    let (mut last, mut replies) = (0, 0);
    loop {
        let path = socket.to_owned();
        let line = tokio::task::spawn_blocking(move || query_status(&path).unwrap())
            .await
            .unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["status"], "running");
        replies += 1;
        match reply["activity"]["state"].as_str().unwrap() {
            "reading_history" => {
                let done = reply["activity"]["records_done"].as_u64().unwrap();
                assert!(done >= last, "the count went back: {reply}");
                last = done;
            }
            "ready" => break,
            other => panic!("unexpected state {other}: {reply}"),
        }
        assert!(replies < 12_000, "the build did not end");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(last > 0, "no reply showed progress in {replies} replies");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn a_first_start_over_ten_thousand_records_answers_status_and_ends_as_a_rebuild() {
    let stand = Stand::new("follow-history").await;
    // 100 sessions of 100 prompts, on the disk before the agent runs.
    write_history(&stand, 100, 100);
    stand.follower(&Feed::new()).pass().unwrap();

    let socket = stand.root.join("run/agent.sock");
    let feed = Feed::new();
    let server = Server::bind(&socket).unwrap().with_activity(feed.clone());
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    let status = |socket: PathBuf| async move {
        let line = tokio::task::spawn_blocking(move || query_status(&socket).unwrap())
            .await
            .unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    assert_eq!(status(socket.clone()).await["activity"]["state"], "waiting");

    // Another writer holds the store's lock, so the build stays in "reading history" for as long
    // as the test wants, however fast the machine is.
    let mut holder = stand.hold_store().await;
    let store = Arc::new(stand.store().await);
    let task = stand.run(&feed, &store);
    wait_phase(&feed, "reading_history", false, Duration::from_secs(30)).await;
    // The build has listed the files once it waits for the lock.
    let mut admin = stand.connect().await;
    let start = Instant::now();
    while sqlx::query_scalar::<_, i64>(
        "select count(*) from pg_locks where locktype = 'advisory' and not granted",
    )
    .fetch_one(&mut admin)
    .await
    .unwrap()
        == 0
    {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the build never waited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A session that starts now is in no file the build listed: only the queue can bring it.
    append(
        &stand.log("new"),
        "2026-11-20T09:00:00Z person /work/late main\n",
    );
    stand.follower(&feed).pass().unwrap();
    for _ in 0..20 {
        let reply = status(socket.clone()).await;
        assert_eq!(reply["status"], "running");
        assert_eq!(reply["activity"]["state"], "reading_history", "{reply}");
        assert!(reply["activity"]["records_done"].is_u64());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    PgStand::release_store(&mut holder).await;
    // The count `status` shows follows the build.
    status_until_ready(&socket).await;
    wait_phase(&feed, "ready", true, Duration::from_secs(120)).await;
    wait_for(&store, Duration::from_secs(20), |s| {
        s.iter().any(|s| s.session == "new" && s.turns == 1)
    })
    .await;

    // What the numbers must be, worked out from the history and not from the store: 100 prompts
    // in turn person, unattended and unknown, every one in a minute of its own.
    let built = contents(&store).await;
    assert_eq!(built.0.len(), 101);
    for s in built.0.iter().filter(|s| s.session.starts_with('s')) {
        assert_eq!(
            (
                s.turns,
                s.person_prompts,
                s.unattended_prompts,
                s.unknown_prompts
            ),
            (100, 34, 33, 33),
            "{}",
            s.session
        );
        assert_eq!(s.active_minutes, 100, "{}", s.session);
    }
    let new = session(&built.0, "new");
    assert_eq!(
        (new.turns, new.person_prompts, new.active_minutes),
        (1, 1, 1)
    );
    // The same, after a rebuild from the files, and the count of records it read.
    let done = std::sync::atomic::AtomicU64::new(0);
    store
        .rebuild_counting(&FileSink::new(&stand.data()), &done)
        .await
        .unwrap();
    assert_eq!(built, contents(&store).await);
    // A turn and its mark, for each of the 10,001 prompts.
    assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 20_002);

    stand.finish(&store, task).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn a_failed_write_is_counted_and_retried_and_a_record_that_keeps_failing_makes_a_rebuild() {
    let stand = Stand::new("follow-retry").await;
    let feed = Feed::new();
    let store = Arc::new(stand.store().await);
    let task = stand.run(&feed, &store);
    wait_phase(&feed, "ready", true, Duration::from_secs(60)).await;
    let mut admin = stand.connect().await;
    let mut follower = stand.follower(&feed);

    // The minutes table is away, so every write fails; it is back before the last try. The
    // record goes to the feed alone and is in no file, so a build would lose it: only a retry
    // brings it in.
    sqlx::query("alter table activity.minutes rename to minutes_away")
        .execute(&mut admin)
        .await
        .unwrap();
    feed.push(&Record::Turn(Turn {
        tools: Vec::new(),
        tool: "fake".into(),
        session: "q".into(),
        time: "2026-11-11T09:00:00Z".into(),
        folder: "/work/q".into(),
        branch: String::new(),
        role: Role::User,
        author: Author::Person,
        text: "only in the queue".into(),
    }));
    let start = Instant::now();
    while feed.to_json()["failed_writes"].as_u64().unwrap_or(0) == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "no failure was counted"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    sqlx::query("alter table activity.minutes_away rename to minutes")
        .execute(&mut admin)
        .await
        .unwrap();
    wait_for(&store, Duration::from_secs(20), |s| {
        s.iter().any(|s| s.session == "q" && s.turns == 1)
    })
    .await;
    wait_phase(&feed, "ready", true, Duration::from_secs(20)).await;

    // Away for longer than the tries last: the store is behind, and a build puts it right.
    sqlx::query("alter table activity.minutes rename to minutes_away")
        .execute(&mut admin)
        .await
        .unwrap();
    append(
        &stand.log("r"),
        "2026-11-11T10:00:00Z person /work/x main\n2026-11-11T10:30:00Z person /work/y main\n",
    );
    follower.pass().unwrap();
    wait_phase(&feed, "behind", false, Duration::from_secs(60)).await;
    sqlx::query("alter table activity.minutes_away rename to minutes")
        .execute(&mut admin)
        .await
        .unwrap();
    wait_phase(&feed, "ready", true, Duration::from_secs(90)).await;
    let built = contents(&store).await;
    assert_eq!(session(&built.0, "r").turns, 2);
    assert_eq!(session(&built.0, "r").active_minutes, 2);

    stand.finish(&store, task).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn the_erasure_leaves_no_activity_row_and_the_store_is_built_again_from_the_old_log() {
    let stand = Stand::new("follow-erase").await;
    let feed = Feed::new();
    let store = Arc::new(stand.store().await);
    let task = stand.run(&feed, &store);
    wait_phase(&feed, "ready", true, Duration::from_secs(60)).await;
    let erase = Erase::new(&stand.data(), &["fake"]);
    let mut follower = stand.follower(&feed).with_erase(erase.clone());
    append(
        &stand.log("s1"),
        "2026-11-11T10:00:05Z person /work/x main\n2026-11-11T10:03:05Z person /work/x main\n",
    );
    follower.pass().unwrap();
    wait_for(&store, Duration::from_secs(10), |s| {
        s.iter().any(|s| s.session == "s1" && s.turns == 2)
    })
    .await;
    assert!(!store.minutes().await.unwrap().is_empty());

    // The list of the stores, as `main` builds it: the files go first, then the rows made from
    // them. Removing the activity time from the list makes this test fail.
    let settings = Settings::open(&stand.root.join("config")).unwrap();
    let manage = Manager::open(&stand.root.join("manage"), settings, Link::default()).unwrap();
    let stores = own_stores(
        &erase,
        None,
        &manage,
        &feed,
        None,
        None,
        &tokio::runtime::Handle::current(),
    );
    let left = tokio::task::spawn_blocking(move || stores.erase())
        .await
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
    assert!(store.sessions().await.unwrap().is_empty());
    assert!(store.minutes().await.unwrap().is_empty());
    assert!(store.is_empty().await.unwrap());

    // The records are gone, and the old log is read again from the start: its sessions are in
    // the store once more, and a line written after that is added once.
    wait_phase(&feed, "ready", true, Duration::from_secs(60)).await;
    follower.pass().unwrap();
    wait_for(&store, Duration::from_secs(10), |s| {
        s.len() == 1 && s[0].turns == 2
    })
    .await;
    append(
        &stand.log("s1"),
        "2026-11-11T11:00:05Z person /work/x main\n",
    );
    follower.pass().unwrap();
    let took = wait_for(&store, Duration::from_secs(10), |s| {
        s.len() == 1 && s[0].turns == 3
    })
    .await;
    assert!(took < Duration::from_secs(10));

    stand.finish(&store, task).await;
}
