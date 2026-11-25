//! Follows the session files of the coding tools, and the stores of the apps kept as snapshots
//! (see [`Snapshot`]), and writes what they say to a [`Sink`].
//!
//! The agent looks at every file by timer, every [`POLL_INTERVAL`], so a new prompt is a
//! record within seconds and the code needs nothing from the platform. A file that was never
//! read and was not changed for 30 days is ignored on every pass; files with
//! a saved offset are always followed. A file whose size and change time are as in the last
//! pass is not opened again. The saved [`Progress`] is the only memory between runs, and the
//! sink drops a replayed record, so a crash between a write and the save loses nothing and
//! doubles nothing. Log files are only opened for reading.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use connectors::{
    ClaudeApp, Conversation, FileState, Line, Parsed, Record, Registry, Snapshot, Tool, Turn,
    for_each_line,
};

use crate::erase::OwnStore;
use crate::records::{FileProgress, FileSink, Progress, Recorder, Sink};

/// A file that was never read is ignored when it was last changed longer ago than this.
const FIRST_SCAN_AGE: Duration = Duration::from_hours(30 * 24);
/// Pause between two passes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Lines read before the progress is saved in the middle of a file.
const SAVE_EVERY_LINES: u32 = 5000;
/// The longest the progress stays unsaved while a pass is running.
const SAVE_EVERY: Duration = Duration::from_secs(2);

/// What `status` tells about one tool.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolStatus {
    /// Turns stored since the agent started; a record the sink had stored already is no turn.
    pub turns: u64,
    /// When the last of those turns was stored; none until one is.
    pub last_turn: Option<SystemTime>,
    /// Lines that did not parse or were too long, since the agent started.
    pub skipped: u64,
    /// True from the start until the first pass has gone through this tool's files: listed
    /// them and tried every one (a file that cannot be read does not hold it). It stays true
    /// while the files cannot be listed.
    pub scanning: bool,
    /// Files that could not be read in the last pass.
    pub unreadable: u64,
    /// The user has this tool, or all collection, switched off: its files are followed so the
    /// saved position moves on, and nothing is stored. After an erasure nothing is read from
    /// the tool at all until it is switched on. As of the last pass.
    pub off: bool,
    /// The app of a snapshot source is not installed, as of the last pass.
    pub not_installed: bool,
    /// Why this tool is not followed fully: its files cannot be listed (or some directories
    /// cannot), the records cannot be written, the progress cannot be saved, the follower did
    /// not start, or it stopped. Several causes are joined with `; `.
    pub error: Option<String>,
}

impl ToolStatus {
    fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "turns": self.turns,
            "skipped": self.skipped,
            "scanning": self.scanning,
        });
        if let Some(fields) = value.as_object_mut() {
            if self.off {
                fields.insert("off".into(), true.into());
            }
            if self.not_installed {
                fields.insert("not_installed".into(), true.into());
            }
            if let Some(at) = self.last_turn.and_then(rfc3339) {
                fields.insert("last_turn".into(), at.into());
            }
            if self.unreadable > 0 {
                fields.insert("unreadable".into(), self.unreadable.into());
            }
            if let Some(error) = &self.error {
                fields.insert("error".into(), error.clone().into());
            }
        }
        value
    }
}

/// A time as UTC in RFC 3339, or none when it cannot be written.
pub(crate) fn rfc3339(at: SystemTime) -> Option<String> {
    OffsetDateTime::from(at).format(&Rfc3339).ok()
}

/// [`ToolStatus`] of every tool, shared between the follower and the status answer.
#[derive(Clone, Default)]
pub struct Stats(Arc<Mutex<BTreeMap<String, ToolStatus>>>);

impl Stats {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, ToolStatus>> {
        crate::sync::lock(&self.0)
    }

    /// Changes one tool's status. `f` runs under the lock: it must not do I/O.
    fn update(&self, tool: &str, f: impl FnOnce(&mut ToolStatus)) {
        f(self.lock().entry(tool.to_owned()).or_default());
    }

    /// Sets or clears the error of a tool; true when that changes what the status shows
    /// (a new message, or a message cleared). The caller prints it, outside the lock.
    fn set_error(&self, tool: &str, error: Option<String>) -> bool {
        let mut changed = false;
        self.update(tool, |s| {
            changed = s.error != error;
            s.error = error;
        });
        changed
    }

    /// Adds a cause beside the error the tool has already.
    fn add_error(&self, tool: &str, cause: &str) {
        self.update(tool, |s| {
            s.error = Some(match s.error.take() {
                Some(known) => format!("{known}; {cause}"),
                None => cause.to_owned(),
            });
        });
    }

    /// Reports a tool that cannot be followed at all, with the reason.
    pub fn not_started(&self, tool: &str, reason: &str) {
        self.set_error(tool, Some(reason.to_owned()));
    }

    /// The per-tool object of the `status` answer.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let tools = self.lock();
        serde_json::Value::Object(
            tools
                .iter()
                .map(|(name, status)| (name.clone(), status.to_json()))
                .collect(),
        )
    }
}

/// Whether `message` is not the last one told about `key`; it is remembered, so the same
/// message is told once however many passes repeat it.
fn is_new(told: &mut HashMap<String, String>, key: &str, message: &str) -> bool {
    if told.get(key).is_some_and(|m| m == message) {
        return false;
    }
    told.insert(key.to_owned(), message.to_owned());
    true
}

/// Tells the user on stderr. A closed stderr is no reason to stop following the logs, which
/// `eprintln!` would be: it panics.
fn say(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
}

/// The text of a panic.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// The collected sessions as a store the person can have erased (see `erase`): every record
/// file of every tool and the saved positions of the readers. The follower keeps the positions
/// in memory too, so this handle and the follower are made as a pair and the follower is given
/// the handle ([`Follower::with_erase`], [`spawn_observed`]).
///
/// The erasure waits for the pass that runs, removes the files, and tells the follower to forget
/// its positions before its next pass. From then on the files of a tool are not read at all
/// while it is switched off: nothing is stored and no position is set. Once it is switched on,
/// its files are read from nothing, as on a fresh install, so the sessions of the last 30 days
/// the tool still has in its own folders are collected again and the dashboard and the memory
/// can be built from scratch.
#[derive(Clone)]
pub struct Erase(Arc<EraseShared>);

struct EraseShared {
    data_dir: PathBuf,
    /// The sources of the tools whose record files this store owns.
    tools: Vec<String>,
    /// Held by the follower for the length of a pass.
    pass: Mutex<()>,
    /// Set by an erasure, taken by the follower at the start of its next pass.
    forget: AtomicBool,
}

impl Erase {
    /// The handle for the follower that stores under `data_dir`. It owns the record files of the
    /// sources `tools` name (the names of the tools it follows) and nothing else under
    /// `records`: another store's files, the chat's conversations, are not its to remove.
    #[must_use]
    pub fn new(data_dir: &Path, tools: &[&str]) -> Self {
        Self(Arc::new(EraseShared {
            data_dir: data_dir.to_owned(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            pass: Mutex::new(()),
            forget: AtomicBool::new(false),
        }))
    }
}

impl Erase {
    fn marker(&self) -> PathBuf {
        self.0.data_dir.join("erase-settling")
    }

    /// Makes the marker, and the entry in the directory, durable. It names, one per line, the
    /// tools that have not been read while switched on since the erasure.
    fn write_marker(&self, waiting: &[String]) -> std::io::Result<()> {
        let temp = self.0.data_dir.join("erase-settling.tmp");
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(waiting.join("\n").as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, self.marker())?;
        std::fs::File::open(&self.0.data_dir)?.sync_all()
    }

    /// The tools the marker names, or none when there is no marker.
    fn waiting(&self, all: &[&'static str]) -> HashSet<&'static str> {
        let text = std::fs::read_to_string(self.marker()).unwrap_or_default();
        all.iter()
            .copied()
            .filter(|tool| text.lines().any(|line| line == *tool))
            .collect()
    }

    /// Notes which tools still wait, once the positions of the others are on the disk; the
    /// marker goes when none does.
    ///
    /// # Errors
    /// The marker could not be written or removed: it keeps naming a tool that no longer
    /// waits, and the next pass tries again.
    fn settle(&self, waiting: &HashSet<&'static str>) -> std::io::Result<()> {
        if waiting.is_empty() {
            return self.clear_marker();
        }
        let mut names: Vec<String> = waiting.iter().map(|t| (*t).to_owned()).collect();
        names.sort();
        let known = std::fs::read_to_string(self.marker()).unwrap_or_default();
        if known.lines().map(str::to_owned).collect::<Vec<_>>() == names {
            return Ok(());
        }
        self.write_marker(&names)
    }

    fn clear_marker(&self) -> std::io::Result<()> {
        match std::fs::remove_file(self.marker()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

impl OwnStore for Erase {
    fn name(&self) -> &'static str {
        "the collected sessions"
    }

    fn erase(&self) -> Result<(), String> {
        let _pass = crate::sync::lock(&self.0.pass);
        // The marker goes first and is made durable: a crash after the files are gone must not
        // leave a restart with neither positions nor marker. The restart drops the positions of
        // the tools the marker names, so what a crash left of them does not hide the sessions.
        match self.write_marker(&self.0.tools) {
            Ok(()) => {}
            // No data directory: there was nothing collected.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err("the erasure could not be noted, so nothing was removed".to_owned());
            }
        }
        FileSink::new(&self.0.data_dir)
            .delete_collected(&self.0.tools)
            .map_err(|_| "the record files could not be removed".to_owned())?;
        self.0.forget.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Tells, by tool name, whether the user lets that tool's sessions be collected. It is asked at
/// before each file is read, so a change of the switch applies from the next file on, not
/// only from the next pass.
pub type Switch = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Whether the switch, when there is one, lets `tool` be stored.
fn is_on(switch: Option<&Switch>, tool: &str) -> bool {
    switch.is_none_or(|on| on(tool))
}

/// Called with every user turn a sink stored for the first time, on the writer's thread, after the
/// turn is in the sink. It must not block.
pub type Observer = Arc<dyn Fn(&Turn) + Send + Sync>;

/// The names of the snapshot sources [`snapshots`] gives.
pub const SNAPSHOT_SOURCES: [&str; 1] = [connectors::CLAUDE_APP];

/// The names of every source the follower reads: the tools of `tools`, then the snapshot sources.
#[must_use]
pub fn source_names(tools: &Registry) -> Vec<&'static str> {
    tools
        .iter()
        .map(Tool::name)
        .chain(SNAPSHOT_SOURCES)
        .collect()
}

/// The snapshot sources the agent reads. Each copies its store into a folder of its own under
/// `data_dir` while it reads it.
#[must_use]
pub fn snapshots(data_dir: &Path) -> Vec<Box<dyn Snapshot>> {
    vec![Box::new(ClaudeApp::new(data_dir.join("claude-app-copy")))]
}

/// Starts a follower of `tools` on a thread of its own, so that neither the first scan nor a
/// slow disk holds up `status`. `paths` is the home directory and the data directory; the
/// follower stops when the returned sender is dropped. When it cannot start (no home, an
/// unreadable progress file), the reason is in the status of every tool.
#[must_use]
pub fn spawn(tools: Registry, paths: io::Result<(PathBuf, PathBuf)>, stats: &Stats) -> Sender<()> {
    spawn_observed(tools, Vec::new(), paths, stats, None, None, None, None)
}

/// Like [`spawn`], with the [`Snapshot`] sources to read besides the tools, a [`Switch`] that
/// decides per source whether its sessions are stored, an [`Observer`] that is told of every new
/// turn, a [`Recorder`] that is told of every new record and the [`Erase`] handle of the
/// person's erasure.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn spawn_observed(
    tools: Registry,
    snapshots: Vec<Box<dyn Snapshot>>,
    paths: io::Result<(PathBuf, PathBuf)>,
    stats: &Stats,
    switch: Option<Switch>,
    observer: Option<Observer>,
    recorder: Option<Recorder>,
    erase: Option<Erase>,
) -> Sender<()> {
    let (alive, stop) = mpsc::channel::<()>();
    let names: Vec<&'static str> = tools
        .iter()
        .map(Tool::name)
        .chain(snapshots.iter().map(|s| s.name()))
        .collect();
    let follower = paths
        .and_then(|(home, data)| {
            let mut sink = FileSink::new(&data);
            if let Some(observer) = observer {
                sink = sink.with_observer(observer);
            }
            if let Some(recorder) = recorder {
                sink = sink.with_recorder(recorder);
            }
            Follower::new(tools, &home, sink, stats.clone())
        })
        .map(|follower| follower.with_snapshots(snapshots))
        .map(|follower| match switch {
            Some(switch) => follower.with_switch(switch),
            None => follower,
        })
        .map(|follower| match erase {
            Some(erase) => follower.with_erase(erase),
            None => follower,
        });
    match follower {
        Ok(follower) => {
            let stats = stats.clone();
            std::thread::spawn(move || {
                // A panic would end the thread unseen while `status` kept saying `running`.
                let ended = panic::catch_unwind(AssertUnwindSafe(|| {
                    follower.run(POLL_INTERVAL, &stop);
                }));
                if let Err(payload) = ended {
                    let reason = format!("follower stopped: {}", panic_text(&*payload));
                    say(&format!("connectors: {reason}"));
                    for name in names {
                        stats.not_started(name, &reason);
                    }
                }
            });
        }
        Err(e) => {
            say(&format!("connectors not started: {e}"));
            for name in names {
                stats.not_started(name, &format!("not started: {e}"));
            }
        }
    }
    alive
}

/// Reads the new lines of every file of every tool, and the new records of every snapshot source.
pub struct Follower<S: Sink> {
    tools: Registry,
    snapshots: Vec<Box<dyn Snapshot>>,
    home: PathBuf,
    sink: S,
    progress: Progress,
    stats: Stats,
    saver: Saver,
    /// Size and change time of each file as the last pass found it.
    checked: HashMap<String, Checked>,
    /// The last read error of each file that has one.
    read_errors: HashMap<String, String>,
    /// Which tools may be stored; all of them when there is none.
    switch: Option<Switch>,
    /// The person's erasure, when the follower is given one.
    erase: Option<Erase>,
    /// The tools and sources that have not been read while switched on since an erasure: while
    /// one is off, it is not read, so no position is saved for it.
    unsettled: HashSet<&'static str>,
}

type Checked = (u64, Option<SystemTime>);

impl<S: Sink> Follower<S> {
    /// A follower that continues from the progress saved in `sink`. All tools start out
    /// as scanning in `stats`.
    ///
    /// # Errors
    /// Fails when the saved progress cannot be read.
    pub fn new(tools: Registry, home: &Path, sink: S, stats: Stats) -> io::Result<Self> {
        let progress = sink.load_progress()?;
        for tool in tools.iter() {
            stats.update(tool.name(), |s| s.scanning = true);
        }
        Ok(Self {
            tools,
            snapshots: Vec::new(),
            home: home.to_owned(),
            sink,
            progress,
            stats,
            saver: Saver::new(),
            checked: HashMap::new(),
            read_errors: HashMap::new(),
            switch: None,
            erase: None,
            unsettled: HashSet::new(),
        })
    }

    /// Reads these sources too, each pass after the tools. Given before [`Follower::with_erase`],
    /// which takes the names of the sources there are by then.
    #[must_use]
    pub fn with_snapshots(mut self, snapshots: Vec<Box<dyn Snapshot>>) -> Self {
        for snapshot in &snapshots {
            self.stats.update(snapshot.name(), |s| s.scanning = true);
        }
        self.snapshots = snapshots;
        self
    }

    /// The names of the tools and of the snapshot sources.
    fn names(&self) -> Vec<&'static str> {
        self.tools
            .iter()
            .map(Tool::name)
            .chain(self.snapshots.iter().map(|s| s.name()))
            .collect()
    }

    /// Lets the person's erasure reach this follower: the pass that follows an erasure starts
    /// from nothing read.
    #[must_use]
    pub fn with_erase(mut self, erase: Erase) -> Self {
        // An erasure that no pass has settled yet, before a restart: a tool that still waits is
        // not read while it is off, and the positions it may have left (a crash in the middle of
        // the erasure, or before they were all saved) are dropped: its files are read from nothing,
        // and a record already in the files is dropped as a replay.
        self.unsettled = erase.waiting(&self.names());
        let roots: Vec<PathBuf> = self
            .tools
            .iter()
            .filter(|tool| self.unsettled.contains(tool.name()))
            .map(|tool| tool.root(&self.home))
            .collect();
        let before = (self.progress.files.len(), self.progress.seen.len());
        self.progress
            .files
            .retain(|path, _| !roots.iter().any(|root| Path::new(path).starts_with(root)));
        let unsettled = &self.unsettled;
        self.progress
            .seen
            .retain(|source, _| !unsettled.contains(source.as_str()));
        self.saver.dirty = (self.progress.files.len(), self.progress.seen.len()) != before;
        self.erase = Some(erase);
        self
    }

    /// Forgets every position, if an erasure said so, and notes that a tool that is off is not
    /// to be read until it is switched on.
    fn forget_if_erased(&mut self) {
        let erased = self
            .erase
            .as_ref()
            .is_some_and(|e| e.0.forget.swap(false, Ordering::SeqCst));
        if erased {
            self.progress = Progress::default();
            self.checked.clear();
            self.read_errors.clear();
            self.saver = Saver::new();
            self.sink.forget();
            for snapshot in &mut self.snapshots {
                snapshot.forget();
            }
            self.unsettled = self.names().into_iter().collect();
        }
    }

    /// Store a tool's sessions only while `switch` says so. A tool that is off is still read,
    /// so its saved position moves on and nothing from the time it was off is stored when it
    /// is switched on again; the records of that time are dropped, not kept for later. The one
    /// exception is a tool that is off after an erasure: it is not read, so it is collected
    /// from the start when it is switched on.
    #[must_use]
    pub fn with_switch(mut self, switch: Switch) -> Self {
        self.switch = Some(switch);
        self
    }

    /// Looks at every file once. A tool whose files cannot be listed, or a file that cannot
    /// be read, is shown in `status` and left for the next pass.
    ///
    /// # Errors
    /// Fails when the sink cannot write or the progress cannot be saved; `status` shows it
    /// too, and the next pass tries again.
    // One pass over every tool; split it when a part grows its own logic.
    #[allow(clippy::too_many_lines)]
    pub fn pass(&mut self) -> io::Result<()> {
        // An erasure waits for this lock, so it never comes in the middle of a pass.
        let erase = self.erase.clone();
        let _pass = erase.as_ref().map(|e| crate::sync::lock(&e.0.pass));
        self.forget_if_erased();
        let names = self.names();
        let Self {
            tools,
            snapshots,
            home,
            sink,
            progress,
            stats,
            saver,
            checked,
            read_errors,
            switch,
            unsettled,
            ..
        } = self;
        let mut failure = None;
        for tool in tools.iter() {
            let off = !is_on(switch.as_ref(), tool.name());
            stats.update(tool.name(), |s| s.off = off);
            let root = tool.root(home);
            let listing = match tool.find_files(&root) {
                Ok(listing) => listing,
                Err(e) => {
                    // Told once, not at every pass; `status` shows it for as long as it lasts.
                    let message = format!("cannot list {}: {e}", root.display());
                    if stats.set_error(tool.name(), Some(message.clone())) {
                        say(&format!("{}: {message}", tool.name()));
                    }
                    // No file is being read: the count of an earlier pass is stale.
                    stats.update(tool.name(), |s| s.unreadable = 0);
                    continue;
                }
            };
            let problem = summary(&listing.problems);
            if stats.set_error(tool.name(), problem.clone())
                && let Some(message) = problem
            {
                say(&format!("{}: {message}", tool.name()));
            }
            let mut unreadable = 0_u64;
            for path in listing.files {
                let mut ctx = Ctx {
                    tool,
                    sink: &mut *sink,
                    progress: &mut *progress,
                    stats,
                    saver: &mut *saver,
                    checked: &mut *checked,
                    switch: switch.as_ref(),
                    unsettled: unsettled.contains(tool.name()),
                };
                let key = path.to_string_lossy().into_owned();
                match ctx.follow(&path) {
                    Ok(()) => {
                        read_errors.remove(&key);
                    }
                    Err(Failure::File(e)) => {
                        unreadable += 1;
                        let message = e.to_string();
                        if is_new(read_errors, &key, &message) {
                            say(&format!("{}: cannot read {key}: {message}", tool.name()));
                        }
                    }
                    Err(Failure::Sink(e)) => {
                        stats.set_error(tool.name(), Some(format!("cannot write records: {e}")));
                        failure = Some(e);
                        break;
                    }
                }
            }
            stats.update(tool.name(), |s| s.unreadable = unreadable);
            if failure.is_some() {
                break;
            }
            // A tool read while it is on has its positions saved from nothing: it is no longer
            // waiting for the switch. One that is off, or was switched off during the pass, keeps
            // waiting. A file that could not be read or listed in that pass has no position, so
            // if it turns readable only while the tool is off it is read and dropped, as under
            // the plain rule.
            if !off && is_on(switch.as_ref(), tool.name()) {
                unsettled.remove(tool.name());
            }
            stats.update(tool.name(), |s| s.scanning = false);
        }
        for snapshot in snapshots.iter_mut() {
            if failure.is_some() {
                break;
            }
            let mut ctx = SnapshotCtx {
                home,
                sink: &mut *sink,
                progress: &mut *progress,
                stats,
                saver: &mut *saver,
                read_errors: &mut *read_errors,
                switch: switch.as_ref(),
                unsettled: &mut *unsettled,
            };
            if let Err(e) = ctx.read(snapshot.as_mut()) {
                failure = Some(e);
            }
        }
        // Kept dirty when it fails, so the next pass saves again.
        let flushed = saver.flush(sink, progress);
        if let Err(e) = &flushed {
            for name in &names {
                stats.add_error(name, &format!("cannot save progress: {e}"));
            }
        }
        // The positions of the tools read while on are on the disk: only the others still wait.
        if flushed.is_ok()
            && failure.is_none()
            && let Some(erase) = &erase
            && let Err(e) = erase.settle(unsettled)
        {
            for name in &names {
                stats.add_error(name, &format!("cannot update the erasure note: {e}"));
            }
        }
        failure.map_or(flushed, Err)
    }

    /// Runs a pass, waits `interval`, and repeats until `stop` is dropped or sent to. A
    /// failing pass is printed once, not at every pass.
    pub fn run(mut self, interval: Duration, stop: &Receiver<()>) {
        let mut told = HashMap::new();
        loop {
            match self.pass() {
                Ok(()) => told.clear(),
                Err(e) => {
                    if is_new(&mut told, "pass", &e.to_string()) {
                        say(&format!("connectors: {e}"));
                    }
                }
            }
            match stop.recv_timeout(interval) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// The first problem, with how many more there are; none when there is none.
fn summary(problems: &[String]) -> Option<String> {
    problems.split_first().map(|(first, rest)| {
        if rest.is_empty() {
            first.clone()
        } else {
            format!("{first} (and {} more)", rest.len())
        }
    })
}

/// Stores a record of `source`, after the mark of its minute when it is a turn, and counts a
/// turn the sink did not have.
fn store<S: Sink>(sink: &mut S, stats: &Stats, source: &str, record: &Record) -> io::Result<()> {
    // Every minute with a log line has a mark, a turn's minute included; the sink keeps one
    // mark per session and minute, so a replay adds nothing.
    if let Record::Turn(turn) = record {
        sink.append(&Record::Mark(turn.mark()))?;
    }
    // A record the sink had stored already is a replay, not a turn read now.
    if sink.append(record)? && matches!(record, Record::Turn(_)) {
        let now = SystemTime::now();
        stats.update(source, |s| {
            s.turns += 1;
            s.last_turn = Some(now);
        });
    }
    Ok(())
}

/// What a pass needs to read one snapshot source.
struct SnapshotCtx<'a, S: Sink> {
    home: &'a Path,
    sink: &'a mut S,
    progress: &'a mut Progress,
    stats: &'a Stats,
    saver: &'a mut Saver,
    read_errors: &'a mut HashMap<String, String>,
    switch: Option<&'a Switch>,
    unsettled: &'a mut HashSet<&'static str>,
}

impl<S: Sink> SnapshotCtx<'_, S> {
    /// Reads a source when it changed. A record whose id was not seen in its session is new, and
    /// is stored while the source is on; while it is off it is only noted as seen, so what was
    /// said meanwhile is never stored. Seen ids are kept until an erasure, also those of records
    /// and sessions the store no longer holds, since the app may load them again. After an
    /// erasure a source that is off is not read at all, so it is collected whole once it is
    /// switched on. A conversation that cannot be read keeps its ids, the others are read, and
    /// the status tells why.
    ///
    /// # Errors
    /// The sink cannot write. Nothing of this read is noted as seen then, so the next pass reads
    /// the source again, and the sink drops what it stored already.
    fn read(&mut self, snapshot: &mut dyn Snapshot) -> io::Result<()> {
        let name = snapshot.name();
        let on = is_on(self.switch, name);
        let installed = snapshot.installed(self.home);
        self.stats.update(name, |s| {
            s.off = !on;
            s.not_installed = !installed;
        });
        if !on && self.unsettled.contains(name) {
            // Nothing to scan while it waits for the switch, as for a tool.
            self.stats.update(name, |s| s.scanning = false);
            return Ok(());
        }
        let conversations = match snapshot.read(self.home) {
            Ok(Some(conversations)) => conversations,
            Ok(None) => {
                self.settle(name, on);
                return Ok(());
            }
            Err(e) => {
                let message = format!("cannot read: {e}");
                if self.stats.set_error(name, Some(message.clone())) {
                    say(&format!("{name}: {message}"));
                }
                return Ok(());
            }
        };
        let known = self.progress.seen.remove(name).unwrap_or_default();
        let mut seen = known.clone();
        let mut unreadable = Vec::new();
        let mut failure = None;
        for Conversation { session, records } in conversations {
            let key = format!("{name}: {session}");
            let records = match records {
                Ok(records) => records,
                Err(e) => {
                    if is_new(self.read_errors, &key, &e) {
                        say(&format!("{name}: cannot read the session {session}: {e}"));
                    }
                    unreadable.push(format!("cannot read the session {session}: {e}"));
                    continue;
                }
            };
            self.read_errors.remove(&key);
            let ids = seen.entry(session).or_default();
            for identified in records {
                if on
                    && !ids.contains(&identified.id)
                    && let Err(e) = store(self.sink, self.stats, name, &identified.record)
                {
                    failure = Some(e);
                    break;
                }
                ids.insert(identified.id);
            }
            if failure.is_some() {
                break;
            }
        }
        if let Some(e) = failure {
            self.progress.seen.insert(name.to_owned(), known);
            snapshot.forget();
            self.stats
                .set_error(name, Some(format!("cannot write records: {e}")));
            return Err(e);
        }
        self.saver.dirty |= seen != known;
        if !seen.is_empty() {
            self.progress.seen.insert(name.to_owned(), seen);
        }
        self.stats.set_error(name, summary(&unreadable));
        self.stats
            .update(name, |s| s.unreadable = unreadable.len() as u64);
        self.settle(name, on);
        Ok(())
    }

    /// The source has been read whole, as of the last read: it is no longer scanning, and, when
    /// it was on for the whole read, no longer waits for the switch after an erasure.
    fn settle(&mut self, name: &'static str, on: bool) {
        if on && is_on(self.switch, name) {
            self.unsettled.remove(name);
        }
        self.stats.update(name, |s| s.scanning = false);
    }
}

enum Failure {
    /// This file cannot be read.
    File(io::Error),
    /// The sink cannot write: stop the pass.
    Sink(io::Error),
}

/// Saves the progress now and then, not after every file: each save is a few `fsync`s.
struct Saver {
    lines: u32,
    last: Instant,
    dirty: bool,
}

impl Saver {
    fn new() -> Self {
        Self {
            lines: 0,
            last: Instant::now(),
            dirty: false,
        }
    }

    fn line_done(&mut self) {
        self.lines += 1;
        self.dirty = true;
    }

    fn due(&self) -> bool {
        self.dirty && (self.lines >= SAVE_EVERY_LINES || self.last.elapsed() >= SAVE_EVERY)
    }

    fn flush<S: Sink>(&mut self, sink: &mut S, progress: &Progress) -> io::Result<()> {
        if self.dirty {
            sink.save_progress(progress)?;
        }
        *self = Self::new();
        Ok(())
    }
}

struct Ctx<'a, S: Sink> {
    tool: &'a dyn Tool,
    sink: &'a mut S,
    progress: &'a mut Progress,
    stats: &'a Stats,
    saver: &'a mut Saver,
    checked: &'a mut HashMap<String, Checked>,
    /// Asked before each file is read: while the user has the tool switched off, lines are
    /// read and nothing is stored.
    switch: Option<&'a Switch>,
    /// This tool was not read while on since an erasure: while it is off, nothing is read, so
    /// no position is set.
    unsettled: bool,
}

impl<S: Sink> Ctx<'_, S> {
    fn follow(&mut self, path: &Path) -> Result<(), Failure> {
        // Asked once per file: the answer decides both whether it is read and whether it is stored.
        let on = is_on(self.switch, self.tool.name());
        let key = path.to_string_lossy().into_owned();
        if !on && self.unsettled {
            return Ok(());
        }
        let meta = std::fs::metadata(path).map_err(Failure::File)?;
        let seen = (meta.len(), meta.modified().ok());
        if self.checked.get(&key) == Some(&seen) {
            return Ok(());
        }
        let (mut offset, mut state) = match self.progress.files.get(&key) {
            Some(p) if p.offset == meta.len() => {
                self.checked.insert(key, seen);
                return Ok(());
            }
            Some(p) if p.offset < meta.len() => (p.offset, p.state.clone()),
            // A shorter file was replaced: it is a new file, whatever it holds so far. The reset
            // is saved before anything is read, so a file that grows past the old offset is
            // not resumed from there after a restart.
            Some(_) => {
                let state = self.tool.open_file(path);
                self.remember(&key, 0, &state);
                self.saver.dirty = true;
                self.saver
                    .flush(self.sink, self.progress)
                    .map_err(Failure::Sink)?;
                (0, state)
            }
            None if meta.len() == 0 || Self::too_old(&meta) => {
                self.checked.insert(key, seen);
                return Ok(());
            }
            None => (0, self.tool.open_file(path)),
        };
        let collect = on;
        let start = offset;
        let mut sink_error = None;
        let read = for_each_line(path, offset, |line, end| {
            let record = match line {
                Line::Text(text) => match self.tool.parse_line(&mut state, &text) {
                    Parsed::Record(r) => Some(r),
                    Parsed::Ignored => None,
                    Parsed::Skipped => {
                        self.count(|s| s.skipped += 1);
                        None
                    }
                },
                Line::TooLong => {
                    self.count(|s| s.skipped += 1);
                    None
                }
            };
            if let Some(record) = record.filter(|_| collect)
                && let Err(e) = store(self.sink, self.stats, self.tool.name(), &record)
            {
                sink_error = Some(e);
                return false;
            }
            offset = end;
            self.saver.line_done();
            if self.saver.due() {
                self.remember(&key, offset, &state);
                if let Err(e) = self.saver.flush(self.sink, self.progress) {
                    sink_error = Some(e);
                    return false;
                }
            }
            true
        });
        // What was read stays read, even when the loop ended on an error.
        if offset != start {
            self.remember(&key, offset, &state);
        }
        if let Some(e) = sink_error {
            return Err(Failure::Sink(e));
        }
        read.map_err(Failure::File)?;
        self.checked.insert(key, seen);
        Ok(())
    }

    fn remember(&mut self, key: &str, offset: u64, state: &FileState) {
        self.progress.files.insert(
            key.to_owned(),
            FileProgress {
                offset,
                state: state.clone(),
            },
        );
    }

    fn too_old(meta: &std::fs::Metadata) -> bool {
        meta.modified()
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > FIRST_SCAN_AGE)
    }

    fn count(&self, f: impl FnOnce(&mut ToolStatus)) {
        self.stats.update(self.tool.name(), f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_new_once_and_again_after_the_entry_is_cleared() {
        let mut told = HashMap::new();
        assert!(is_new(&mut told, "a", "denied"));
        assert!(!is_new(&mut told, "a", "denied"));
        assert!(is_new(&mut told, "a", "gone"));
        assert!(is_new(&mut told, "b", "gone"));
        told.remove("a");
        assert!(is_new(&mut told, "a", "gone"));
    }
}
