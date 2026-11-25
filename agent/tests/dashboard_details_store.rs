//! The socket commands `dashboard-sessions`, `dashboard-summary` and `dashboard-state` on a real
//! activity store, built from record files. The tests start PostgreSQL, so they are ignored by
//! default: `cargo test -- --ignored`.

#![cfg(target_os = "linux")]
// Test code: a failed `unwrap` is a failed test.
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::pg_stand::PgStand;
use connectors::{Author, Record, Role, Turn};
use construct_agent::activity::Feed;
use construct_agent::dashboard::{ACTIVITY_FILE, default_activity};
use construct_agent::dashboard_details::Details;
use construct_agent::dashboard_document::Dashboard;
use construct_agent::ipc::{Server, request};
use construct_agent::records::{FileSink, Sink};
use construct_agent::settings::{Settings, Update};
use construct_agent::workspace::Workspace;
use serde_json::{Value, json};

fn prompt(session: &str, time: &str, folder: &str, branch: &str) -> Record {
    Record::Turn(Turn {
        tool: "claude".into(),
        session: session.into(),
        time: time.into(),
        folder: folder.into(),
        branch: branch.into(),
        role: Role::User,
        author: Author::Person,
        text: "words nobody may see".into(),
        tools: Vec::new(),
    })
}

fn ask(socket: &Path, line: &str) -> Value {
    let reply = request(socket, line, Duration::from_secs(10)).unwrap();
    serde_json::from_str(&reply).unwrap()
}

/// Two sessions on one day, one of them on a branch named after a ticket.
fn write_records(data: &Path) {
    let mut sink = FileSink::new(data);
    for (session, folder, branch, hour) in [
        ("first-session-id", "/work/shop", "SHOP-7-cart", "09"),
        ("second-session-id", "/work/blog", "main", "10"),
    ] {
        for minute in ["00:10", "01:10", "02:10"] {
            let time = format!("2026-11-10T{hour}:{minute}Z");
            sink.append(&prompt(session, &time, folder, branch))
                .unwrap();
        }
    }
}

struct Stand {
    pg: PgStand,
    socket: std::path::PathBuf,
    feed: Feed,
    settings: Settings,
}

async fn start(name: &str, records: bool) -> Stand {
    let pg = PgStand::new(name).await;
    if records {
        write_records(&pg.root.join("data/records"));
    }
    let workspace = Arc::new(Workspace::open(&pg.root.join("workspace")).unwrap());
    workspace
        .write(ACTIVITY_FILE, &default_activity().to_string())
        .unwrap();
    let dashboard = Dashboard::open(Arc::clone(&workspace));
    let settings = Settings::open(&pg.root.join("config")).unwrap();
    let feed = Feed::new();
    let server = Server::bind(&pg.root.join("agent.sock"))
        .unwrap()
        .with_activity(feed.clone())
        .with_dashboard(dashboard.clone())
        .with_details(Details::new(
            feed.clone(),
            dashboard,
            Some(settings.clone()),
            tokio::runtime::Handle::current(),
        ));
    let socket = server.path().to_path_buf();
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    Stand {
        pg,
        socket,
        feed,
        settings,
    }
}

async fn call(socket: &Path, line: String) -> Value {
    let socket = socket.to_path_buf();
    tokio::task::spawn_blocking(move || ask(&socket, &line))
        .await
        .unwrap()
}

/// Builds the store from the record files and waits until it can be read.
async fn build(
    stand: &Stand,
) -> (
    Arc<construct_agent::activity::Store>,
    tokio::task::JoinHandle<()>,
) {
    let store = Arc::new(stand.pg.store().await);
    let task = {
        let (feed, store, data) = (
            stand.feed.clone(),
            store.clone(),
            stand.pg.root.join("data/records"),
        );
        tokio::spawn(async move { feed.run(&store, &FileSink::new(&data)).await })
    };
    let start = Instant::now();
    loop {
        let state = call(&stand.socket, "dashboard-state".to_owned()).await;
        if state["state"] != "reading" {
            return (store, task);
        }
        assert!(start.elapsed() < Duration::from_secs(60), "{state}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const DAY: &str =
    r#"{ "label": "2026-11-10", "start": "2026-11-10T00:00:00Z", "end": "2026-11-11T00:00:00Z" }"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn the_socket_answers_the_details_of_a_dashboard_from_the_store() {
    let stand = start("details-store", true).await;

    // Before the store is built the state is reading, and the other answers say so.
    let early = call(&stand.socket, "dashboard-state".to_owned()).await;
    assert_eq!(
        early,
        json!({ "state": "reading", "records_done": 0, "tools_off": [] })
    );
    let sessions =
        format!(r#"dashboard-sessions {{"group_by":"project","group":"shop","days":[{DAY}]}}"#);
    assert_eq!(
        call(&stand.socket, sessions.clone()).await,
        json!({ "error": "the activity time is being built" })
    );

    let (store, task) = build(&stand).await;
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"codex":false}}"#).unwrap())
        .unwrap();
    assert_eq!(
        call(&stand.socket, "dashboard-state".to_owned()).await,
        json!({ "state": "ready", "tools_off": ["codex", "chatgpt"] })
    );

    let page = call(&stand.socket, sessions).await;
    assert_eq!(page["total"], 1);
    assert_eq!(page["sessions"][0]["project"], "shop");
    assert_eq!(page["sessions"][0]["ticket"], "SHOP-7");
    assert_eq!(page["sessions"][0]["session"], "first-se");
    assert_eq!(page["sessions"][0]["start"], "2026-11-10T09:00:00Z");

    let summary = call(
        &stand.socket,
        format!(r#"dashboard-summary {{"widget":"end-of-day","day":{DAY}}}"#),
    )
    .await;
    assert_eq!(
        summary["lines"],
        json!(["No ticket (blog): 3 min", "SHOP-7: 3 min"])
    );

    for answer in [&page, &summary] {
        assert!(!answer.to_string().contains("nobody may see"));
    }

    task.abort();
    let _ = task.await;
    store.close().await;
    stand.pg.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts PostgreSQL; run with --ignored"]
async fn the_state_is_empty_when_there_are_no_records() {
    let stand = start("details-empty", false).await;
    let (store, task) = build(&stand).await;
    assert_eq!(
        call(&stand.socket, "dashboard-state".to_owned()).await,
        json!({ "state": "empty", "tools_off": [] })
    );
    stand
        .settings
        .apply(Update::parse(r#"{"collection":{"enabled":false}}"#).unwrap())
        .unwrap();
    assert_eq!(
        call(&stand.socket, "dashboard-state".to_owned()).await,
        json!({ "state": "empty", "tools_off": ["claude-code", "codex", "chatgpt", "claude-app"] })
    );
    task.abort();
    let _ = task.await;
    store.close().await;
    stand.pg.finish().await;
}
