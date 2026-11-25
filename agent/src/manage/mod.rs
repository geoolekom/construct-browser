//! Sends what the user said to the Construct host, so the profile is built from it: every user
//! prompt a record store takes in, from the log readers or from the chat, goes to the host's `manage-facts` route over its loopback port,
//! with the turns before it as context. The route plans with the model inside the request, so
//! prompts go one at a time from a queue on a thread of their own, and the readers never wait.
//!
//! A prompt typed after the agent's first start is *live* and goes out by itself. An older one is
//! history and goes out only when the user runs `backfill`, because every prompt costs a model
//! call. A live prompt goes before the rest of a backfill. It waits for the call in progress, and
//! after a failed or refused prompt for the pause that follows (`Timing`).
//!
//! The journal (see [`journal`]) keeps what was sent, so a restart, a crash or a second
//! `backfill` never sends a prompt twice. A prompt the host may have processed counts as sent
//! (`status` counts it as `unsure`). Prompt text is never logged, journaled or put in `status`.
//!
//! [`input`] shapes a call, [`call`] makes it and reads the answer, and [`journal`] keeps what was
//! sent. This module holds what they share: the queue, the thread, `backfill` and the counters.
//!
//! Every prompt is checked against [`Settings::collects`] before it is queued and again before it
//! is sent, so with collection off nothing is sent. A prompt typed while collection is off is
//! never sent: it is marked as refused in the journal, and `status` counts it as `refused`. A
//! prompt that collection refuses later, one queued when it went off or one from history, is not
//! marked: a live one is queued again at the next start, a history one goes out in a backfill, and
//! a backfill that is running stops. The exception is the person's erasure (`memory-erase`): it
//! marks every live prompt stored so far as refused, so none of them goes out later.

mod call;
mod input;
mod journal;
mod person;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use connectors::{Registry, Role, Turn, compare_times};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::chat::Link;
use crate::erase::{OwnStore, OwnStores};
use crate::host::Connection;
use crate::ipc::{create_private_dir, error_response};
use crate::model::Target;
use crate::records::{FileSink, Sink, fnv};
use crate::settings::{Settings, Update};
use crate::sync::lock;
use call::{Answer, Reply, call};
use input::input;
use journal::{Journal, days_ago, first_start};

pub const CONTEXT_TURNS: usize = 10;
/// Days of history `backfill` looks at when none is given.
const DEFAULT_DAYS: u32 = 30;
/// Calls on one prompt before it is given up for now.
const ATTEMPTS: usize = 3;
/// Rounds of failed calls on a live prompt before it is left to the next start.
const MAX_STRIKES: u32 = 3;
/// Prompts of a backfill that fail one after the other before the run stops.
const MAX_FAILED_IN_A_ROW: u32 = 3;

/// How long the thread waits and pauses; the tests make it short.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Pauses between the calls on one prompt.
    pub retry: [Duration; ATTEMPTS - 1],
    /// How often the thread looks for a host or a model while it has none.
    pub idle: Duration,
    /// The pause after a prompt failed, for every prompt that follows. A new prompt does not
    /// cut it short.
    pub failed: Duration,
    /// The pause after the host refused the agent.
    pub refused: Duration,
    /// How long one call may take; the route's own limit is shorter.
    pub call: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            retry: [Duration::from_secs(2), Duration::from_secs(10)],
            idle: Duration::from_secs(2),
            failed: Duration::from_secs(60),
            refused: Duration::from_secs(300),
            call: Duration::from_secs(45),
        }
    }
}

/// `lines` joined into messages, each of at most [`input::MESSAGE_LIMIT`] characters when a line
/// is short enough, and none empty.
fn pack(lines: &[String]) -> Vec<String> {
    let mut messages: Vec<String> = Vec::new();
    for line in lines.iter().filter(|l| !l.trim().is_empty()) {
        match messages.last_mut() {
            Some(last)
                if last.chars().count() + 1 + line.chars().count() <= input::MESSAGE_LIMIT =>
            {
                last.push('\n');
                last.push_str(line);
            }
            _ => messages.push(line.clone()),
        }
    }
    messages
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Prompt {
    tool: String,
    session: String,
    time: String,
    text: String,
}

impl Prompt {
    fn of(turn: &Turn) -> Self {
        Self {
            tool: turn.tool.clone(),
            session: turn.session.clone(),
            time: turn.time.clone(),
            text: turn.text.clone(),
        }
    }

    fn key(&self) -> u64 {
        fnv(&[&self.tool, &self.session, &self.time, &self.text])
    }
}

/// Which queue a prompt comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Live,
    Backfill,
}

#[derive(Default)]
struct Queue {
    live: VecDeque<Prompt>,
    /// Keys of the live prompts in `live`, so a prompt is never queued twice.
    queued: HashSet<u64>,
    /// The prompts of the backfill that runs, oldest first.
    backfill: Option<VecDeque<Prompt>>,
}

#[derive(Default)]
struct Counts {
    live_sent: u64,
    live_stored: u64,
    live_failed: u64,
    /// Prompts the host may have processed and the agent does not know.
    unsure: u64,
    added: u64,
    error: Option<String>,
    backfill: BackfillCounts,
}

#[derive(Default)]
struct BackfillCounts {
    /// `idle` until a run started, then `running`, `done` or `stopped`.
    state: &'static str,
    total: u64,
    sent: u64,
    stored: u64,
    failed: u64,
    error: Option<String>,
}

/// The arguments of `backfill`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct BackfillRequest {
    days: u32,
    limit: Option<usize>,
    dry_run: bool,
}

impl Default for BackfillRequest {
    fn default() -> Self {
        Self {
            days: DEFAULT_DAYS,
            limit: None,
            dry_run: false,
        }
    }
}

struct Inner {
    data_dir: PathBuf,
    first_start: String,
    tools: Vec<&'static str>,
    settings: Settings,
    link: Link,
    timing: Timing,
    journal: Arc<Mutex<Journal>>,
    queue: Mutex<Queue>,
    wake: Condvar,
    counts: Mutex<Counts>,
    /// Held while the agent reads or changes the person's settings in the host, so an erasure
    /// and a switch-on never overlap.
    person: Mutex<()>,
    /// Keys of the live prompts that were stored at any erasure of the person: one that was being
    /// sent at that moment and then fails is not given back for a later start.
    erased: Mutex<HashSet<u64>>,
    /// The agent's own stores, erased with the person (see `erase`); set once at start.
    own: OnceLock<OwnStores>,
    /// Tells the paired phones that everything was erased; set once at start (see `erase`).
    phones: OnceLock<PhoneReset>,
}

/// What `memory-erase` calls to tell the paired phones. An error is a fixed plain reason.
pub type PhoneReset = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// The journal of the sent prompts as a store the person can have erased.
struct JournalStore(Arc<Mutex<Journal>>);

impl OwnStore for JournalStore {
    fn name(&self) -> &'static str {
        "the journal of the sent prompts"
    }

    fn erase(&self) -> Result<(), String> {
        lock(&self.0)
            .erase()
            .map_err(|_| "the journal could not be emptied".to_owned())
    }
}

/// The prompts the user typed, sent to the host. Cheap to clone.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

impl Manager {
    /// The manager over the files in `data_dir`. The first start is noted here, if it is the first.
    ///
    /// # Errors
    /// Fails when the directory or its files cannot be created or read.
    pub fn open(data_dir: &Path, settings: Settings, link: Link) -> io::Result<Self> {
        Self::open_with(data_dir, settings, link, Timing::default())
    }

    /// Like [`Manager::open`], with these pauses; for the tests.
    ///
    /// # Errors
    /// Fails when the directory or its files cannot be created or read.
    pub fn open_with(
        data_dir: &Path,
        settings: Settings,
        link: Link,
        timing: Timing,
    ) -> io::Result<Self> {
        create_private_dir(data_dir)?;
        let mut tools = crate::follow::source_names(&Registry::builtin());
        tools.push(crate::chat::SOURCE);
        Ok(Self {
            inner: Arc::new(Inner {
                data_dir: data_dir.to_owned(),
                first_start: first_start(data_dir)?,
                tools,
                settings,
                link,
                timing,
                journal: Arc::new(Mutex::new(Journal::open(data_dir)?)),
                queue: Mutex::new(Queue::default()),
                wake: Condvar::new(),
                counts: Mutex::new(Counts::default()),
                person: Mutex::new(()),
                erased: Mutex::new(HashSet::new()),
                own: OnceLock::new(),
                phones: OnceLock::new(),
            }),
        })
    }

    /// The journal of the sent prompts, for the list of the agent's own stores.
    #[must_use]
    pub fn journal_store(&self) -> Arc<dyn OwnStore> {
        Arc::new(JournalStore(Arc::clone(&self.inner.journal)))
    }

    /// The stores `memory-erase` clears besides the host's copy. Set once; a second call is
    /// ignored.
    pub fn set_own_stores(&self, stores: OwnStores) {
        let _ = self.inner.own.set(stores);
    }

    /// How `memory-erase` tells the paired phones. Set once; a second call is ignored.
    pub fn set_phone_reset(&self, reset: PhoneReset) {
        let _ = self.inner.phones.set(reset);
    }

    /// Whether everything about the person is being erased now: `memory-erase` is running, or
    /// the host is still erasing. Nothing is read from the agent's own stores meanwhile. When
    /// the host cannot be asked (not linked yet, a failed or slow read), it is not known to be erasing:
    /// the facts come from the same host and are unavailable then, and the chats were emptied
    /// before the host was asked.
    #[must_use]
    pub fn erasing(&self) -> bool {
        let _held = match self.inner.person.try_lock() {
            Err(std::sync::TryLockError::WouldBlock) => return true,
            Ok(held) => held,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        self.inner
            .link
            .get()
            .and_then(|connection| person::read(&connection).ok())
            .is_some_and(|gear| gear.erasure_in_progress)
    }

    /// What a record store calls with every new user turn (the log readers' and the chat's): a
    /// prompt from after the first start is queued. It never blocks.
    #[must_use]
    pub fn observer(&self) -> crate::follow::Observer {
        let inner = Arc::clone(&self.inner);
        // The stores call it for user turns only.
        Arc::new(move |turn: &Turn| inner.offer(Prompt::of(turn)))
    }

    /// Starts the thread that sends the queued prompts.
    pub fn spawn(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::spawn(move || inner.run());
    }

    /// The `backfill` command: `argument` is a JSON object with `days`, `limit` and `dry_run`.
    /// The answer is one JSON line.
    #[must_use]
    pub fn backfill(&self, argument: &str) -> String {
        let request = if argument.trim().is_empty() {
            Ok(BackfillRequest::default())
        } else {
            serde_json::from_str::<BackfillRequest>(argument)
        };
        let Ok(request) = request else {
            return error_response("the arguments of backfill are not valid");
        };
        if request.days == 0 || request.limit == Some(0) {
            return error_response("days and limit must be positive");
        }
        match self.inner.backfill(&request) {
            Ok(reply) => reply.to_string(),
            Err(message) => error_response(&message),
        }
    }

    /// The `memory-erase` command: collection goes off, then the host is asked to erase the
    /// person. The host answers at once and erases in the background. While an erasure runs,
    /// the request changes nothing. The answer is one JSON line.
    #[must_use]
    pub fn erase(&self) -> String {
        let _held = lock(&self.inner.person);
        let Some(connection) = self.inner.link.get() else {
            return error_response("the Construct host is not ready yet");
        };
        match person::read(&connection) {
            Err(reason) => return error_response(&reason),
            Ok(gear) if gear.erasure_in_progress => {
                return json!({ "erasure": "running", "collection_enabled": self.collection_on() })
                    .to_string();
            }
            Ok(_) => {}
        }
        // Off first: from here on nothing new is sent to the host, whatever happens next.
        if self.set_collection(false).is_err() {
            return error_response("the settings file could not be written, so nothing was erased");
        }
        // The phones are told from here, before anything is deleted: whatever happens next, each
        // of them empties itself. Their checks are refused while the erasure runs (see `erasing`).
        if let Some(reset) = self.inner.phones.get()
            && reset().is_err()
        {
            return error_response(
                "collection is off, but the paired phones could not be told, so nothing was erased",
            );
        }
        // What was typed before and not yet sent must not come back after a switch-on.
        self.inner.forget_unsent();
        // The agent's own copies go too, also when the host's erasure does not start.
        let left = self
            .inner
            .own
            .get()
            .map(OwnStores::erase)
            .unwrap_or_default();
        let host = person::request_erasure(&connection);
        match (host, left.as_slice()) {
            (Ok(()), []) => {
                json!({ "erasure": "started", "collection_enabled": false }).to_string()
            }
            (Ok(()), left) => error_response(&format!(
                "collection is off and the erasure started, but the agent could not erase: {}; \
                 run it again when the erasure has ended",
                left.join(", ")
            )),
            (Err(reason), []) => error_response(&format!(
                "collection is off, but the erasure did not start: {reason}"
            )),
            (Err(reason), left) => error_response(&format!(
                "collection is off, but the erasure did not start: {reason}; the agent could not erase: {}",
                left.join(", ")
            )),
        }
    }

    /// Change the settings. Switching all collection from off to on first turns personalization
    /// on in the host, because an erasure leaves it off, and only then lets the readers collect.
    /// It is refused while an erasure runs, and when the host cannot be asked: collection then
    /// stays off. The error is a fixed plain reason.
    ///
    /// # Errors
    /// The reason the change was refused.
    pub fn apply_settings(&self, update: Update) -> Result<(), String> {
        if !update.turns_collection_on() {
            return self
                .inner
                .settings
                .apply(update)
                .map_err(|e| e.as_str().into());
        }
        let _held = lock(&self.inner.person);
        // Read under the lock: an erasure that ran meanwhile has switched collection off.
        if self.collection_on() {
            return self
                .inner
                .settings
                .apply(update)
                .map_err(|e| e.as_str().into());
        }
        let Some(connection) = self.inner.link.get() else {
            return Err("collection stays off: the Construct host is not ready yet".into());
        };
        let gear = person::read(&connection)
            .map_err(|reason| format!("collection stays off: {reason}"))?;
        if gear.erasure_in_progress {
            return Err("collection stays off: an erasure is still running".into());
        }
        if !gear.personalization_enabled {
            person::turn_personalization_on(&connection)
                .map_err(|reason| format!("collection stays off: {reason}"))?;
        }
        self.inner
            .settings
            .apply(update)
            .map_err(|e| e.as_str().into())
    }

    fn collection_on(&self) -> bool {
        self.inner.settings.collection().enabled
    }

    fn set_collection(&self, on: bool) -> Result<(), crate::settings::Error> {
        let update = json!({ "collection": { "enabled": on } }).to_string();
        Update::parse(&update).and_then(|update| self.inner.settings.apply(update))
    }

    /// What `status` shows as `facts`: counts and states, never a prompt.
    #[must_use]
    pub fn status(&self) -> Value {
        self.inner.status()
    }

    /// Sends what the user's own setup found out (one statement to a line) through the same route
    /// as a prompt, so it is kept as facts: the lines are packed into messages of at most
    /// [`input::MESSAGE_LIMIT`] characters, and each is sent at once, not queued. `go` is asked
    /// before each message, and the sending ends when it answers `false`. The route compares what
    /// it is sent with the facts it holds, so a statement sent again is not stored twice.
    #[must_use]
    pub fn remember(&self, lines: &[String], go: &dyn Fn() -> bool) -> Remembered {
        self.inner.remember(lines, go)
    }
}

/// What [`Manager::remember`] did: how many facts the host stored, and why the sending ended
/// before the last message, when it did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Remembered {
    pub stored: u64,
    pub error: Option<String>,
}

/// Why a prompt did not go out.
enum Failure {
    /// The calls did not get an answer worth keeping; a later call may.
    Unavailable(String),
    /// The host refuses every call of the agent.
    Refused(String),
    /// The host refuses this prompt, and would refuse it again.
    Rejected(String),
}

#[derive(Clone, Copy)]
enum Done {
    Stored {
        added: u64,
    },
    Nothing,
    /// The host may have processed the prompt: it is not sent again.
    Unsure,
    /// Not sent: it was sent before, or holds no text.
    Skipped,
}

impl Inner {
    /// A prompt that was just stored; see the module doc for what is refused.
    fn offer(&self, prompt: Prompt) {
        if !self.is_live(&prompt) {
            return;
        }
        if !self.settings.collects(&prompt.tool) {
            if let Err(e) = lock(&self.journal).refuse(prompt.key()) {
                self.note_error(&format!("cannot write the journal: {e}"));
            }
            return;
        }
        self.enqueue(prompt);
    }

    /// Whether the prompt was typed after the agent's first start.
    fn is_live(&self, prompt: &Prompt) -> bool {
        compare_times(&prompt.time, &self.first_start).is_ge()
    }

    /// Queues a live prompt.
    fn enqueue(&self, prompt: Prompt) {
        let key = prompt.key();
        if lock(&self.journal).is_done(key) {
            return;
        }
        let mut queue = lock(&self.queue);
        if queue.queued.insert(key) {
            queue.live.push_back(prompt);
            drop(queue);
            self.wake.notify_all();
        }
    }

    /// The user prompts of every tool, oldest first. It reads every record file.
    fn prompts(&self) -> io::Result<Vec<Prompt>> {
        let sink = FileSink::new(&self.data_dir);
        let mut all = Vec::new();
        for tool in &self.tools {
            all.extend(sink.user_turns(tool)?.iter().map(Prompt::of));
        }
        all.sort_by(|a, b| compare_times(&a.time, &b.time));
        Ok(all)
    }

    /// Drops the queued live prompts and marks every live prompt that was stored and not sent as
    /// refused, so that no later start or switch-on sends what the person asked to erase.
    fn forget_unsent(&self) {
        {
            let mut queue = lock(&self.queue);
            queue.live.clear();
            queue.queued.clear();
        }
        match self.prompts() {
            Ok(all) => {
                let mut journal = lock(&self.journal);
                let mut erased = lock(&self.erased);
                for prompt in all.iter().filter(|p| self.is_live(p)) {
                    erased.insert(prompt.key());
                    if let Err(e) = journal.refuse(prompt.key()) {
                        self.note_error(&format!("cannot write the journal: {e}"));
                    }
                }
            }
            Err(e) => self.note_error(&format!("cannot read the records: {e}")),
        }
    }

    /// Queues the live prompts that were stored but not sent before the agent stopped.
    fn recover(&self) {
        match self.prompts() {
            Ok(all) => all
                .into_iter()
                .filter(|p| self.is_live(p) && self.settings.collects(&p.tool))
                .for_each(|p| self.enqueue(p)),
            Err(e) => self.note_error(&format!("cannot read the records: {e}")),
        }
    }

    fn model_is_set(&self) -> bool {
        Target::from_settings(&self.settings.model()).is_some()
    }

    fn backfill(&self, request: &BackfillRequest) -> Result<Value, String> {
        let since = days_ago(request.days).map_err(|e| e.to_string())?;
        // The files are read before the journal is locked: the readers and `status` use it.
        let all = self
            .prompts()
            .map_err(|e| format!("cannot read the records: {e}"))?;
        let candidates: Vec<Prompt> = {
            let journal = lock(&self.journal);
            all.into_iter()
                .filter(|p| {
                    self.settings.collects(&p.tool)
                        && compare_times(&p.time, &since).is_ge()
                        && compare_times(&p.time, &self.first_start).is_lt()
                        && !journal.is_done(p.key())
                })
                .collect()
        };
        let mut per_tool: BTreeMap<&str, usize> =
            self.tools.iter().map(|tool| (*tool, 0)).collect();
        for p in &candidates {
            if let Some(n) = per_tool.get_mut(p.tool.as_str()) {
                *n += 1;
            }
        }
        let total = candidates.len();
        let chosen = request.limit.map_or(total, |limit| limit.min(total));
        if request.dry_run {
            return Ok(json!({
                "dry_run": true,
                "days": request.days,
                "prompts": per_tool,
                "total": total,
                "would_send": chosen,
            }));
        }
        if self.link.get().is_none() {
            return Err("Construct is not running yet".into());
        }
        if !self.model_is_set() {
            return Err("no model is set: save its address, name and key in the settings".into());
        }
        let mut queue = lock(&self.queue);
        if queue.backfill.as_ref().is_some_and(|left| !left.is_empty()) {
            return Err("a backfill is running; see `status`".into());
        }
        queue.backfill = Some(candidates.into_iter().take(chosen).collect());
        drop(queue);
        lock(&self.counts).backfill = BackfillCounts {
            state: if chosen == 0 { "done" } else { "running" },
            total: chosen as u64,
            ..BackfillCounts::default()
        };
        self.wake.notify_all();
        Ok(json!({
            "started": chosen > 0,
            "days": request.days,
            "prompts": per_tool,
            "queued": chosen,
        }))
    }

    fn status(&self) -> Value {
        let (queued, backfill_left) = {
            let queue = lock(&self.queue);
            (
                queue.live.len(),
                queue.backfill.as_ref().map_or(0, VecDeque::len),
            )
        };
        let (sent, refused) = {
            let journal = lock(&self.journal);
            (journal.sent.len(), journal.refused.len())
        };
        let counts = lock(&self.counts);
        let b = &counts.backfill;
        let mut backfill = json!({
            "state": if b.state.is_empty() { "idle" } else { b.state },
            "total": b.total,
            "sent": b.sent,
            "stored": b.stored,
            "failed": b.failed,
            "left": backfill_left,
        });
        if let Some(error) = &b.error {
            backfill["error"] = error.clone().into();
        }
        let mut facts = json!({
            "since": self.first_start,
            "sent": sent,
            "added": counts.added,
            "unsure": counts.unsure,
            "refused": refused,
            "live": {
                "queued": queued,
                "sent": counts.live_sent,
                "stored": counts.live_stored,
                "failed": counts.live_failed,
            },
            "backfill": backfill,
        });
        if let Some(error) = &counts.error {
            facts["error"] = error.clone().into();
        }
        facts
    }

    fn note_error(&self, message: &str) {
        lock(&self.counts).error = Some(message.to_owned());
    }

    /// Waits for a new prompt or a backfill, at most `idle`.
    fn wait_for_work(&self) {
        let queue = lock(&self.queue);
        let _ = self.wake.wait_timeout(queue, self.timing.idle);
    }

    /// The prompt to send next, live ones first. It stays in its queue until it is settled.
    fn next(&self) -> Option<(Source, Prompt)> {
        let queue = lock(&self.queue);
        if let Some(p) = queue.live.front() {
            return Some((Source::Live, p.clone()));
        }
        queue
            .backfill
            .as_ref()
            .and_then(VecDeque::front)
            .map(|p| (Source::Backfill, p.clone()))
    }

    fn remove(&self, source: Source, key: u64) {
        let mut queue = lock(&self.queue);
        match source {
            Source::Live => {
                queue.live.pop_front();
                queue.queued.remove(&key);
            }
            Source::Backfill => {
                if let Some(left) = queue.backfill.as_mut() {
                    left.pop_front();
                }
            }
        }
    }

    fn run(&self) {
        self.recover();
        // Failed rounds of each live prompt in the queue.
        let mut strikes: HashMap<u64, u32> = HashMap::new();
        let mut failed_in_a_row = 0_u32;
        loop {
            let Some(connection) = self.link.get() else {
                self.wait_for_work();
                continue;
            };
            if !self.model_is_set() {
                self.wait_for_work();
                continue;
            }
            let Some((source, prompt)) = self.next() else {
                self.wait_for_work();
                continue;
            };
            let key = prompt.key();
            if !self.settings.collects(&prompt.tool) {
                // Collection went off while it waited; see the module doc.
                self.remove(source, key);
                if source == Source::Backfill {
                    self.stop_backfill("collection is off");
                }
                continue;
            }
            match self.send(&connection, &prompt) {
                Ok(done) => {
                    strikes.remove(&key);
                    failed_in_a_row = 0;
                    self.remove(source, key);
                    self.settled(source, done);
                }
                Err(Failure::Unavailable(reason)) => {
                    self.note_error(&reason);
                    match source {
                        Source::Live => {
                            let strike = strikes.entry(key).or_default();
                            *strike += 1;
                            if *strike >= MAX_STRIKES {
                                // Left for the next start, which queues it again.
                                strikes.remove(&key);
                                self.remove(source, key);
                                lock(&self.counts).live_failed += 1;
                            }
                        }
                        Source::Backfill => {
                            // The rest of a backfill would fail the same way: stop it after a few.
                            self.remove(source, key);
                            failed_in_a_row += 1;
                            lock(&self.counts).backfill.failed += 1;
                            if failed_in_a_row >= MAX_FAILED_IN_A_ROW {
                                self.stop_backfill(&reason);
                                failed_in_a_row = 0;
                            }
                        }
                    }
                    // A pause after a failure is for the host to recover: nothing cuts it short.
                    std::thread::sleep(self.timing.failed);
                }
                Err(Failure::Refused(reason)) => {
                    self.note_error(&reason);
                    if source == Source::Backfill {
                        self.stop_backfill(&reason);
                    }
                    std::thread::sleep(self.timing.refused);
                }
                Err(Failure::Rejected(reason)) => {
                    // The mark stays: the host refused this prompt as such. A host that refuses
                    // every prompt (a changed schema) is stopped after a few, not drained.
                    self.note_error(&reason);
                    self.remove(source, key);
                    failed_in_a_row += 1;
                    {
                        let mut counts = lock(&self.counts);
                        match source {
                            Source::Live => counts.live_failed += 1,
                            Source::Backfill => counts.backfill.failed += 1,
                        }
                        self.finish_backfill(source, &mut counts);
                    }
                    if failed_in_a_row >= MAX_FAILED_IN_A_ROW {
                        if source == Source::Backfill {
                            self.stop_backfill(&reason);
                        }
                        failed_in_a_row = 0;
                        std::thread::sleep(self.timing.failed);
                    }
                }
            }
        }
    }

    fn stop_backfill(&self, reason: &str) {
        lock(&self.queue).backfill = None;
        let mut counts = lock(&self.counts);
        counts.backfill.state = "stopped";
        counts.backfill.error = Some(reason.to_owned());
    }

    fn settled(&self, source: Source, done: Done) {
        let mut counts = lock(&self.counts);
        let (stored, added) = match done {
            Done::Stored { added } => (true, added),
            Done::Nothing => (false, 0),
            Done::Unsure => {
                counts.unsure += 1;
                (false, 0)
            }
            Done::Skipped => {
                self.finish_backfill(source, &mut counts);
                return;
            }
        };
        counts.added += added;
        match source {
            Source::Live => {
                counts.live_sent += 1;
                counts.live_stored += u64::from(stored);
            }
            Source::Backfill => {
                counts.backfill.sent += 1;
                counts.backfill.stored += u64::from(stored);
            }
        }
        self.finish_backfill(source, &mut counts);
        counts.error = None;
    }

    /// Marks a backfill done when its last prompt is out of the queue.
    fn finish_backfill(&self, source: Source, counts: &mut Counts) {
        if source != Source::Backfill {
            return;
        }
        let finished = lock(&self.queue)
            .backfill
            .as_ref()
            .is_none_or(VecDeque::is_empty);
        if finished && counts.backfill.state == "running" {
            counts.backfill.state = "done";
        }
    }

    /// The context without what was typed while collection was off: those prompts and the answers
    /// to them stay on the machine, like the prompts themselves.
    fn without_refused(&self, turns: Vec<Turn>) -> Vec<Turn> {
        let journal = lock(&self.journal);
        let mut skipping = false;
        turns
            .into_iter()
            .filter(|t| {
                if t.role == Role::User {
                    skipping = journal.refused.contains(&Prompt::of(t).key());
                }
                !skipping
            })
            .collect()
    }

    /// One prompt: its context is read, it is marked in the journal, and then up to [`ATTEMPTS`]
    /// calls are made.
    fn send(&self, connection: &Connection, prompt: &Prompt) -> Result<Done, Failure> {
        if prompt.text.trim().is_empty() {
            return Ok(Done::Skipped);
        }
        // A chat conversation that was deleted while its message waited: nothing to send.
        if prompt.tool == crate::chat::SOURCE
            && FileSink::new(&self.data_dir)
                .read_session(&prompt.tool, &prompt.session)
                .map_err(|e| Failure::Unavailable(format!("cannot read the records: {e}")))?
                .is_empty()
        {
            return Ok(Done::Skipped);
        }
        // Before the mark: a prompt whose context cannot be read is not sent without it.
        // The whole session is read and cleaned before the last turns are taken, so a window never
        // opens in the middle of an answer to a prompt that was refused.
        let mut before = FileSink::new(&self.data_dir)
            .turns_before(&prompt.tool, &prompt.session, &prompt.time, usize::MAX)
            .map_err(|e| Failure::Unavailable(format!("cannot read the records: {e}")))?;
        before = self.without_refused(before);
        before.drain(..before.len().saturating_sub(CONTEXT_TURNS));
        let key = prompt.key();
        match lock(&self.journal).begin(key) {
            Ok(true) => {}
            // Sent already, by the other queue or before a restart.
            Ok(false) => return Ok(Done::Skipped),
            Err(e) => {
                return Err(Failure::Unavailable(format!(
                    "cannot write the journal: {e}"
                )));
            }
        }
        let (message, conversation) = input(&prompt.text, &before);
        let body = json!({ "message": message, "conversation": conversation }).to_string();
        match self.call_again(connection, &body) {
            Reply::Answered(Answer::Stored { added }) => Ok(Done::Stored { added }),
            Reply::Answered(Answer::Nothing) => Ok(Done::Nothing),
            // A call that may have run is not sent again.
            Reply::MaybeProcessed => Ok(Done::Unsure),
            Reply::HostRefused(reason) => {
                self.take_back(key);
                Err(Failure::Refused(reason))
            }
            Reply::PromptRefused(reason) => Err(Failure::Rejected(reason)),
            Reply::Answered(Answer::Failed(reason)) | Reply::NotDelivered(reason) => {
                self.take_back(key);
                Err(Failure::Unavailable(reason))
            }
        }
    }

    /// One call, made up to [`ATTEMPTS`] times while the host does not take it: an answer that is
    /// final (stored, nothing to store, a refusal, a result that is unknown) ends it, and the
    /// last reason is the answer of calls that all failed, as `NotDelivered`.
    fn call_again(&self, connection: &Connection, body: &str) -> Reply {
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            match call(connection, body, self.timing.call) {
                Reply::Answered(Answer::Failed(reason)) | Reply::NotDelivered(reason) => {
                    last = reason;
                }
                done => return done,
            }
            if let Some(pause) = self.timing.retry.get(attempt) {
                std::thread::sleep(*pause);
            }
        }
        Reply::NotDelivered(last)
    }

    fn remember(&self, lines: &[String], go: &dyn Fn() -> bool) -> Remembered {
        let mut done = Remembered::default();
        if !self.settings.collects(crate::chat::SOURCE) {
            done.error = Some("collection is off".into());
            return done;
        }
        let Some(connection) = self.link.get() else {
            done.error = Some("Construct is not running yet".into());
            return done;
        };
        for message in pack(lines) {
            if !go() {
                done.error = Some("the sending was stopped".into());
                break;
            }
            match self.say(&connection, &message) {
                Ok(added) => done.stored += added,
                Err(reason) => {
                    done.error = Some(reason);
                    break;
                }
            }
        }
        done
    }

    /// One message to the route, tried up to [`ATTEMPTS`] times when the host did not take it.
    fn say(&self, connection: &Connection, message: &str) -> Result<u64, String> {
        let (message, conversation) = input(message, &[]);
        let body = json!({ "message": message, "conversation": conversation }).to_string();
        match self.call_again(connection, &body) {
            Reply::Answered(Answer::Stored { added }) => Ok(added),
            Reply::Answered(Answer::Nothing) => Ok(0),
            Reply::MaybeProcessed => Err("the host did not say whether it stored the facts".into()),
            Reply::HostRefused(reason)
            | Reply::PromptRefused(reason)
            | Reply::Answered(Answer::Failed(reason))
            | Reply::NotDelivered(reason) => Err(reason),
        }
    }

    fn take_back(&self, key: u64) {
        let mut journal = lock(&self.journal);
        // Read under the journal lock, which `forget_unsent` also holds while it records the keys:
        // an erase either comes before this read, or finds the prompt marked as sent and not given
        // back yet, and then this call sees its key.
        let erased = lock(&self.erased).contains(&key);
        // A prompt that was being sent when the person erased is not given back: it would be
        // queued again at the next start.
        let result = journal
            .undo(key)
            .and_then(|()| if erased { journal.refuse(key) } else { Ok(()) });
        if let Err(e) = result {
            drop(journal);
            self.note_error(&format!("cannot write the journal: {e}"));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // a failed `unwrap` in a test is a failed test
mod tests {
    use super::*;

    fn manager(name: &str) -> Manager {
        let dir = std::env::temp_dir().join(format!("cb-manage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = Settings::open(&dir.join("config")).unwrap();
        Manager::open_with(
            &dir.join("data"),
            settings,
            Link::default(),
            Timing::default(),
        )
        .unwrap()
    }

    fn prompt() -> Prompt {
        Prompt {
            tool: "claude-code".into(),
            session: "s".into(),
            time: "2099-01-01T00:00:00Z".into(),
            text: "t".into(),
        }
    }

    #[test]
    fn an_erasure_during_a_send_makes_a_failing_prompt_refused_not_given_back() {
        let manager = manager("forget-in-flight");
        let inner = &manager.inner;
        let mut stored = prompt();
        stored.time = "2099-01-01T00:00:00Z".into();
        let turn = Turn {
            tool: stored.tool.clone(),
            session: stored.session.clone(),
            time: stored.time.clone(),
            folder: String::new(),
            branch: String::new(),
            role: Role::User,
            author: connectors::Author::Unknown,
            text: stored.text.clone(),
            tools: Vec::new(),
        };
        let mut sink = FileSink::new(&inner.data_dir);
        sink.append(&connectors::Record::Turn(turn)).unwrap();
        sink.save_progress(&crate::records::Progress::default())
            .unwrap();

        // The prompt is being sent when the person erases, and its calls then fail.
        let key = stored.key();
        assert!(lock(&inner.journal).begin(key).unwrap());
        inner.forget_unsent();
        inner.take_back(key);
        let journal = lock(&inner.journal);
        assert!(journal.is_done(key));
        assert!(!journal.sent.contains(&key));
    }

    #[test]
    fn a_prompt_whose_calls_fail_is_given_back_unless_the_person_erased_meanwhile() {
        let manager = manager("take-back");
        let inner = &manager.inner;
        let key = prompt().key();

        // Sent, then given back: it can be queued again at the next start.
        assert!(lock(&inner.journal).begin(key).unwrap());
        inner.take_back(key);
        assert!(!lock(&inner.journal).is_done(key));

        // The same prompt in flight when the person erased: it ends refused, never given back.
        assert!(lock(&inner.journal).begin(key).unwrap());
        lock(&inner.erased).insert(key);
        inner.take_back(key);
        let journal = lock(&inner.journal);
        assert!(journal.is_done(key));
        assert!(!journal.sent.contains(&key));
    }

    #[test]
    fn lines_are_packed_into_messages_within_the_limit_and_empty_ones_are_dropped() {
        let line = "x".repeat(input::MESSAGE_LIMIT / 2 - 1);
        let lines = vec![
            line.clone(),
            String::new(),
            line.clone(),
            "last".to_owned(),
            "  ".to_owned(),
        ];
        let messages = pack(&lines);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].chars().count() <= input::MESSAGE_LIMIT);
        assert_eq!(messages[0], format!("{line}\n{line}"));
        assert_eq!(messages[1], "last");
        assert!(pack(&[]).is_empty());
    }
}
