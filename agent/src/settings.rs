//! The user's settings: one file, `settings.json`, in the config directory, and one shared
//! object that gives the current values to the other parts of the agent.
//!
//! The file is user-only (`0600` from the first byte) and is replaced in one step by
//! `host::write_private`: the new content goes to a user-only temporary file in the same
//! directory, then the temporary file is renamed over the old one. A crash in between leaves
//! the old file as it was.
//!
//! Errors carry fixed texts only. A rejected `settings-set` request holds the API key, so
//! no error ever repeats any part of the request.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub use chat_core::model::Model;
use chat_core::model::{check_base_url, check_key};
use serde::{Deserialize, Serialize};

use crate::ipc::absolute_env_path;

/// Settings file name inside the `construct` config directory.
pub const FILE_NAME: &str = "settings.json";
/// Format version written into the file.
pub const VERSION: u32 = 1;

/// The tools whose sessions are read, by the name their sessions carry in the store.
pub const TOOLS: [&str; 4] = ["claude-code", "codex", "chatgpt", connectors::CLAUDE_APP];

/// What the readers collect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
// One flag per switch: these are the field names of the settings file and of the answer.
#[allow(clippy::struct_excessive_bools)]
pub struct Collection {
    pub enabled: bool,
    pub claude_code: bool,
    pub codex: bool,
    /// The screen samples (see `screen`): off until the person switches them on.
    pub screen: bool,
}

impl Default for Collection {
    fn default() -> Self {
        Self {
            enabled: true,
            claude_code: true,
            codex: true,
            screen: false,
        }
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // the signature `skip_serializing_if` wants
fn is_false(value: &bool) -> bool {
    !*value
}

/// The content of the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    version: u32,
    model: Model,
    collection: Collection,
    /// Set when the file was unreadable and the agent started with collection off; cleared when
    /// the user sets `collection.enabled`. The app shows an information badge while it is set.
    #[serde(skip_serializing_if = "is_false")]
    recovered: bool,
}

impl Default for Data {
    fn default() -> Self {
        Self {
            version: VERSION,
            model: Model::default(),
            collection: Collection::default(),
            recovered: false,
        }
    }
}

impl Data {
    /// What the agent starts with when the file could not be read: nothing is collected until
    /// the user switches collection on again, because the unreadable file may have held an off switch.
    fn unreadable() -> Self {
        Self {
            collection: Collection {
                enabled: false,
                ..Collection::default()
            },
            recovered: true,
            ..Self::default()
        }
    }
}

/// Why a settings request was refused. The texts are fixed on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The request is not the JSON object this command takes.
    InvalidRequest,
    /// The model address is not an `https` address, nor `http` on `localhost` or `127.0.0.1`,
    /// or it has a user name or password in it.
    InvalidBaseUrl,
    /// The key has a control character, or a key and `remove_key` came together.
    InvalidKey,
    /// The file could not be written.
    Storage,
}

impl Error {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Error::InvalidRequest => "invalid settings request",
            Error::InvalidBaseUrl => {
                "base_url must be an https address, or http on localhost or 127.0.0.1, without a user name or password"
            }
            Error::InvalidKey => "invalid api_key",
            Error::Storage => "the settings file could not be written",
        }
    }
}

impl From<chat_core::model::Error> for Error {
    fn from(error: chat_core::model::Error) -> Self {
        match error {
            chat_core::model::Error::InvalidBaseUrl => Error::InvalidBaseUrl,
            chat_core::model::Error::InvalidKey => Error::InvalidKey,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Error {}

/// The fields a `settings-set` request may change. Anything not listed is refused.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Update {
    model: Option<ModelUpdate>,
    collection: Option<CollectionUpdate>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ModelUpdate {
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    remove_key: bool,
}

impl fmt::Debug for ModelUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelUpdate")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key_set", &self.api_key.is_some())
            .field("remove_key", &self.remove_key)
            .finish()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct CollectionUpdate {
    enabled: Option<bool>,
    claude_code: Option<bool>,
    codex: Option<bool>,
    screen: Option<bool>,
}

impl Update {
    /// Whether the request switches all collection on.
    #[must_use]
    pub fn turns_collection_on(&self) -> bool {
        self.collection
            .as_ref()
            .is_some_and(|c| c.enabled == Some(true))
    }

    /// Read a request body.
    ///
    /// # Errors
    /// [`Error::InvalidRequest`] when `body` is not a JSON object of known fields.
    pub fn parse(body: &str) -> Result<Self, Error> {
        serde_json::from_str(body).map_err(|_| Error::InvalidRequest)
    }
}

/// The config directory: `$XDG_CONFIG_HOME/construct` (default `~/.config/construct`) on
/// Linux, `~/Library/Application Support/construct` on macOS. Pure, so tests pass the
/// environment in.
#[must_use]
pub fn config_dir_from(
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    macos: bool,
) -> Option<PathBuf> {
    let base = if macos {
        home?.join("Library").join("Application Support")
    } else {
        match xdg_config_home {
            Some(dir) => dir,
            None => home?.join(".config"),
        }
    };
    Some(base.join("construct"))
}

/// The config directory of this user, from the environment.
///
/// # Errors
/// Fails when `HOME` is not set to an absolute path and `XDG_CONFIG_HOME` does not help.
pub fn config_dir() -> io::Result<PathBuf> {
    config_dir_from(
        absolute_env_path("XDG_CONFIG_HOME"),
        absolute_env_path("HOME"),
        cfg!(target_os = "macos"),
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

/// Apply `update` to `data`; `data` is untouched on an error.
fn merged(data: &Data, update: Update) -> Result<Data, Error> {
    let mut next = data.clone();
    if let Some(m) = update.model {
        if let Some(url) = m.base_url {
            check_base_url(&url)?;
            next.model.base_url = url;
        }
        if let Some(name) = m.model {
            next.model.model = name;
        }
        let new_key = m.api_key.filter(|k| !k.is_empty());
        if m.remove_key {
            if new_key.is_some() {
                return Err(Error::InvalidKey);
            }
            next.model.api_key = None;
        } else if let Some(key) = new_key {
            check_key(&key)?;
            next.model.api_key = Some(key);
        }
    }
    if let Some(c) = update.collection {
        // Setting the switch, either way, is the user's answer to the notice.
        if c.enabled.is_some() {
            next.recovered = false;
        }
        let target = &mut next.collection;
        target.enabled = c.enabled.unwrap_or(target.enabled);
        target.claude_code = c.claude_code.unwrap_or(target.claude_code);
        target.codex = c.codex.unwrap_or(target.codex);
        target.screen = c.screen.unwrap_or(target.screen);
    }
    Ok(next)
}

/// The settings, shared. Clones see the same values, so a change made over the socket is
/// visible to every reader at once.
#[derive(Clone, Debug)]
pub struct Settings {
    path: Arc<PathBuf>,
    data: Arc<Mutex<Data>>,
    /// Whether this computer can give the screen signals; set by the sampler, which knows.
    screen_supported: Arc<AtomicBool>,
}

impl Settings {
    /// Load the file in the config directory `dir`. A missing file gives the defaults. A
    /// file that cannot be read as settings (broken, or from a newer version) is copied to
    /// `settings.json.corrupt`, so a later change cannot destroy it, and replaced by a new
    /// file with collection switched off: the broken file may have held an off switch, and
    /// nothing is collected until the user switches collection on again.
    ///
    /// # Errors
    /// Fails when the file exists and cannot be read or copied aside, or the new file cannot
    /// be saved; the unreadable file is then still in place.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join(FILE_NAME);
        let (data, unreadable) = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Data>(&bytes) {
                Ok(data) if data.version <= VERSION => (data, false),
                _ => {
                    // Copy it aside first and let the save below replace it in one step: while
                    // that save has not happened the broken file stays in place, so a failed
                    // save is met by the same unreadable file at the next start, never by a
                    // missing one that would read as the defaults (collection on).
                    let mut aside = path.as_os_str().to_owned();
                    aside.push(".corrupt");
                    fs::copy(&path, aside)?;
                    eprintln!(
                        "the settings file could not be read; kept a copy aside, collection is off"
                    );
                    (Data::unreadable(), true)
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Data::default(), false),
            Err(e) => return Err(e),
        };
        let settings = Self {
            path: Arc::new(path),
            data: Arc::new(Mutex::new(data.clone())),
            screen_supported: Arc::new(AtomicBool::new(false)),
        };
        if unreadable {
            settings.save(&data)?;
        }
        Ok(settings)
    }

    /// Tells whether this computer can give the screen signals, as the settings answer says.
    pub fn set_screen_supported(&self, supported: bool) {
        self.screen_supported.store(supported, Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, Data> {
        crate::sync::lock(&self.data)
    }

    /// The current model, key included. For the model client only; never to the socket.
    #[must_use]
    pub fn model(&self) -> Model {
        self.lock().model.clone()
    }

    #[must_use]
    pub fn collection(&self) -> Collection {
        self.lock().collection
    }

    /// Whether the sessions of the source called `tool` (a connector's name, or the chat's) may be stored:
    /// collection is on, and so is the tool's own switch. A tool the settings do not know is off.
    /// The chat and the Claude app have no switch of their own: they follow collection alone.
    #[must_use]
    pub fn collects(&self, tool: &str) -> bool {
        let c = self.collection();
        c.enabled
            && match tool {
                "claude-code" => c.claude_code,
                // The Codex switch also answers for the ChatGPT app's chats.
                "codex" | "chatgpt" => c.codex,
                crate::screen::SOURCE => c.screen,
                crate::chat::SOURCE | connectors::CLAUDE_APP => true,
                _ => false,
            }
    }

    /// The tools whose sessions are not stored now: all of them while collection is off, else the
    /// ones with their own switch off.
    #[must_use]
    pub fn tools_off(&self) -> Vec<&'static str> {
        TOOLS
            .into_iter()
            .filter(|tool| !self.collects(tool))
            .collect()
    }

    /// Change settings and save them. The change is visible to every reader once this
    /// returns `Ok`; on an error nothing changed, in memory or on disk.
    ///
    /// # Errors
    /// An [`Error`] naming the refused field, or [`Error::Storage`].
    pub fn apply(&self, update: Update) -> Result<(), Error> {
        let mut data = self.lock();
        let next = merged(&data, update)?;
        if next == *data {
            return Ok(());
        }
        self.save(&next).map_err(|e| {
            // The kind (disk full, permission denied, ...) holds no key; the message could hold a path.
            eprintln!("could not save the settings: {}", e.kind());
            Error::Storage
        })?;
        *data = next;
        Ok(())
    }

    fn save(&self, data: &Data) -> io::Result<()> {
        let content = serde_json::to_vec_pretty(data).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            crate::ipc::create_private_dir(dir)?;
        }
        crate::host::write_private(&self.path, &content)
    }

    /// The `settings-get` answer: no key, only whether one is set.
    #[must_use]
    pub fn get_response(&self) -> String {
        let data = self.lock();
        serde_json::json!({
            "model": {
                "base_url": data.model.base_url,
                "model": data.model.model,
                "api_key_set": data.model.api_key.is_some(),
            },
            "collection": data.collection,
            // What this computer can collect.
            "supports": { "screen": self.screen_supported.load(Ordering::SeqCst) },
            // For the information badge: why collection is off.
            "notice": data.recovered.then_some("settings file was unreadable; collection is off"),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cb-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn update(json: &str) -> Update {
        Update::parse(json).unwrap()
    }

    #[test]
    fn defaults_when_there_is_no_file() {
        let dir = scratch("defaults");
        let s = Settings::open(&dir).unwrap();
        assert_eq!(s.model(), Model::default());
        assert_eq!(s.collection(), Collection::default());
        assert!(s.collection().enabled && s.collection().claude_code && s.collection().codex);
        assert!(!dir.join(FILE_NAME).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_set_and_a_get_survive_a_reopen() {
        let dir = scratch("setget");
        let s = Settings::open(&dir).unwrap();
        s.apply(update(
            r#"{"model":{"base_url":"https://api.example.com/v1","model":"m1","api_key":"k-1"},
                "collection":{"codex":false}}"#,
        ))
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&s.get_response()).unwrap();
        assert_eq!(v["model"]["base_url"], "https://api.example.com/v1");
        assert_eq!(v["model"]["model"], "m1");
        assert_eq!(v["model"]["api_key_set"], true);
        assert_eq!(v["collection"]["codex"], false);
        assert_eq!(v["collection"]["claude_code"], true);

        let again = Settings::open(&dir).unwrap();
        assert_eq!(again.model().api_key.as_deref(), Some("k-1"));
        assert_eq!(again.model().model, "m1");
        assert!(!again.collection().codex);
        let file: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(FILE_NAME)).unwrap()).unwrap();
        assert_eq!(file["version"], VERSION);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tool_is_collected_only_while_collection_and_its_switch_are_on() {
        let dir = scratch("collects");
        let s = Settings::open(&dir).unwrap();
        assert!(s.collects("claude-code") && s.collects("codex"));
        assert!(!s.collects("unknown-tool"));
        assert!(s.collects("chat"));
        s.apply(update(
            r#"{"collection":{"claude_code":false,"codex":false}}"#,
        ))
        .unwrap();
        assert!(s.collects("chat") && s.collects("claude-app"));
        s.apply(update(
            r#"{"collection":{"claude_code":true,"codex":true}}"#,
        ))
        .unwrap();
        assert!(s.collects("chatgpt"));
        s.apply(update(r#"{"collection":{"codex":false}}"#))
            .unwrap();
        assert!(s.collects("claude-code") && !s.collects("codex"));
        // The ChatGPT app's chats follow the Codex switch.
        assert!(!s.collects("chatgpt"));
        assert_eq!(s.tools_off(), ["codex", "chatgpt"]);
        s.apply(update(r#"{"collection":{"enabled":false,"codex":true}}"#))
            .unwrap();
        assert!(!s.collects("claude-code") && !s.collects("codex") && !s.collects("chatgpt"));
        assert!(!s.collects("claude-app"));
        assert_eq!(
            s.tools_off(),
            ["claude-code", "codex", "chatgpt", "claude-app"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_with_no_screen_reads_as_off() {
        let dir = scratch("no-screen");
        fs::write(
            dir.join(FILE_NAME),
            r#"{"version":1,"collection":{"enabled":true,"claude_code":true,"codex":false}}"#,
        )
        .unwrap();
        let s = Settings::open(&dir).unwrap();
        assert!(!s.collection().screen);
        assert!(!s.collects("screen"));
        assert!(!s.collection().codex);
        let v: serde_json::Value = serde_json::from_str(&s.get_response()).unwrap();
        assert_eq!(v["collection"]["screen"], false);
        assert_eq!(v["supports"]["screen"], false);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_screen_is_collected_only_while_all_collection_and_its_switch_are_on() {
        let dir = scratch("screen-switch");
        let s = Settings::open(&dir).unwrap();
        assert!(!s.collects("screen"));
        s.apply(update(r#"{"collection":{"screen":true}}"#))
            .unwrap();
        assert!(s.collects("screen"));
        s.apply(update(r#"{"collection":{"enabled":false}}"#))
            .unwrap();
        assert!(!s.collects("screen"));
        assert!(Settings::open(&dir).unwrap().collection().screen);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_change_is_visible_to_every_clone_at_once() {
        let dir = scratch("shared");
        let s = Settings::open(&dir).unwrap();
        let reader = s.clone();
        s.apply(update(
            r#"{"model":{"model":"m2"},"collection":{"enabled":false}}"#,
        ))
        .unwrap();
        assert_eq!(reader.model().model, "m2");
        assert!(!reader.collection().enabled);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_or_empty_key_keeps_the_saved_one() {
        let dir = scratch("keepkey");
        let s = Settings::open(&dir).unwrap();
        s.apply(update(r#"{"model":{"api_key":"secret-1"}}"#))
            .unwrap();
        s.apply(update(r#"{"model":{"model":"m3"}}"#)).unwrap();
        s.apply(update(r#"{"model":{"api_key":""}}"#)).unwrap();
        assert_eq!(s.model().api_key.as_deref(), Some("secret-1"));
        assert_eq!(s.model().model, "m3");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_key_leaves_no_key_in_the_file() {
        let dir = scratch("removekey");
        let s = Settings::open(&dir).unwrap();
        s.apply(update(r#"{"model":{"api_key":"secret-2"}}"#))
            .unwrap();
        assert!(
            fs::read_to_string(dir.join(FILE_NAME))
                .unwrap()
                .contains("secret-2")
        );
        s.apply(update(r#"{"model":{"remove_key":true}}"#)).unwrap();
        let text = fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(!text.contains("secret-2"), "{text}");
        assert!(!text.contains("api_key\": \""), "{text}");
        assert_eq!(s.model().api_key, None);
        assert_eq!(Settings::open(&dir).unwrap().model().api_key, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_key_together_with_remove_key_is_refused() {
        let dir = scratch("both");
        let s = Settings::open(&dir).unwrap();
        let err = s
            .apply(update(r#"{"model":{"api_key":"k","remove_key":true}}"#))
            .unwrap_err();
        assert_eq!(err, Error::InvalidKey);
        let _ = fs::remove_dir_all(dir);
    }

    fn tmp_file(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME).with_extension("tmp")
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_user_only_from_the_first_byte() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        // A leftover temporary file with a wide mode must not be reused for the key.
        fs::write(tmp_file(&dir), b"old").unwrap();
        fs::set_permissions(tmp_file(&dir), fs::Permissions::from_mode(0o644)).unwrap();

        let s = Settings::open(&dir).unwrap();
        s.apply(update(r#"{"model":{"api_key":"k"}}"#)).unwrap();
        assert_eq!(mode(&dir.join(FILE_NAME)), 0o600);
        assert!(!tmp_file(&dir).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_crash_between_the_temporary_file_and_the_rename_keeps_the_old_file() {
        let dir = scratch("crash");
        let s = Settings::open(&dir).unwrap();
        s.apply(update(r#"{"model":{"model":"old"}}"#)).unwrap();
        let before = fs::read(dir.join(FILE_NAME)).unwrap();

        // The agent dies after step one: the temporary file is written, the rename never runs.
        fs::write(tmp_file(&dir), b"half written").unwrap();
        assert_eq!(fs::read(dir.join(FILE_NAME)).unwrap(), before);

        // The next start reads the old file, and the next save replaces the leftover.
        let again = Settings::open(&dir).unwrap();
        assert_eq!(again.model().model, "old");
        again.apply(update(r#"{"model":{"model":"new"}}"#)).unwrap();
        assert_eq!(Settings::open(&dir).unwrap().model().model, "new");
        assert!(!tmp_file(&dir).exists());
        let _ = fs::remove_dir_all(dir);
    }

    /// Make every save in `dir` fail: a directory sits where the temporary file goes.
    fn block_saves(dir: &Path) {
        fs::create_dir_all(tmp_file(dir)).unwrap();
        fs::write(tmp_file(dir).join("x"), b"").unwrap();
    }

    #[test]
    fn a_failed_save_changes_nothing() {
        let dir = scratch("failsave");
        let s = Settings::open(&dir).unwrap();
        block_saves(&dir);
        let err = s.apply(update(r#"{"model":{"model":"m"}}"#)).unwrap_err();
        assert_eq!(err, Error::Storage);
        assert_eq!(s.model().model, "");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_off_save_leaves_the_unreadable_file_so_the_next_start_is_still_off() {
        let dir = scratch("failoff");
        fs::write(dir.join(FILE_NAME), b"{ not json").unwrap();
        block_saves(&dir);
        // The save of the off file fails: there are no settings, and the broken file is
        // still where it was, not gone.
        assert!(Settings::open(&dir).is_err());
        assert_eq!(fs::read(dir.join(FILE_NAME)).unwrap(), b"{ not json");
        assert!(Settings::open(&dir).is_err());

        // Once saving works again the next start comes up with collection off.
        fs::remove_dir_all(tmp_file(&dir)).unwrap();
        let s = Settings::open(&dir).unwrap();
        assert!(!s.collection().enabled);
        assert!(!Settings::open(&dir).unwrap().collection().enabled);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn addresses_are_checked_by_parsed_host() {
        let dir = scratch("urls");
        let s = Settings::open(&dir).unwrap();
        let set = |url: &str| {
            s.apply(
                Update::parse(&serde_json::json!({"model": {"base_url": url}}).to_string())
                    .unwrap(),
            )
        };
        for ok in [
            "https://api.example.com/v1",
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "https://localhost",
            "",
        ] {
            assert_eq!(set(ok), Ok(()), "{ok}");
        }
        for bad in [
            "http://example.com",
            "http://localhost.example.com",
            "http://localhost.example.com/v1",
            "http://127.0.0.1.example.com",
            "http://localhostevil.com",
            "http://[::1]:8080",
            "http://0.0.0.0",
            "https://user:pass@example.com",
            "https://user@example.com",
            "http://user:pass@localhost",
            "http://localhost@example.com",
            "ftp://example.com",
            "file:///etc/passwd",
            "example.com",
            "https://",
        ] {
            assert_eq!(set(bad), Err(Error::InvalidBaseUrl), "{bad}");
        }
        // A refused address leaves the saved one in place.
        assert_eq!(s.model().base_url, "");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_request_is_refused_with_a_fixed_error() {
        for body in [
            r#"{"model":{"api_key":"sk-top-secret","nope":1}}"#,
            r#"{"model":{"api_key":42}}"#,
            r#"{"model":{"api_key":"sk-top-secret""#,
            r#"["sk-top-secret"]"#,
        ] {
            assert_eq!(Update::parse(body).unwrap_err(), Error::InvalidRequest);
        }
        let dir = scratch("badkey");
        let s = Settings::open(&dir).unwrap();
        let err = s
            .apply(update(r#"{"model":{"api_key":"sk-top\nsecret"}}"#))
            .unwrap_err();
        assert_eq!(err, Error::InvalidKey);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn debug_output_hides_the_key() {
        let dir = scratch("debug");
        let s = Settings::open(&dir).unwrap();
        s.apply(update(r#"{"model":{"api_key":"sk-top-secret"}}"#))
            .unwrap();
        assert!(!format!("{s:?}").contains("sk-top-secret"));
        assert!(!format!("{:?}", s.model()).contains("sk-top-secret"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_file_is_kept_aside_and_replaced_by_an_off_file() {
        let dir = scratch("broken");
        fs::write(dir.join(FILE_NAME), b"{ not json").unwrap();
        let s = Settings::open(&dir).unwrap();
        assert_eq!(s.model(), Model::default());
        // The broken file may have held an off switch: nothing is collected until the user
        // switches collection on again.
        assert!(!s.collection().enabled);
        let notice = |s: &Settings| {
            let v: serde_json::Value = serde_json::from_str(&s.get_response()).unwrap();
            v["notice"].clone()
        };
        assert!(notice(&s).is_string());
        assert!(notice(&Settings::open(&dir).unwrap()).is_string());
        // Changing something else does not answer the notice.
        s.apply(update(r#"{"model":{"model":"m"}}"#)).unwrap();
        assert!(notice(&s).is_string());
        assert_eq!(
            fs::read(dir.join("settings.json.corrupt")).unwrap(),
            b"{ not json"
        );
        // ... and that holds after a restart, not only until it.
        assert!(!Settings::open(&dir).unwrap().collection().enabled);
        s.apply(update(r#"{"collection":{"enabled":true}}"#))
            .unwrap();
        assert!(s.collection().enabled);
        assert!(notice(&s).is_null());
        assert!(notice(&Settings::open(&dir).unwrap()).is_null());

        let _ = fs::remove_dir_all(dir);

        // A file from a newer version gets the same treatment, in a directory of its own.
        let dir = scratch("newer");
        fs::write(dir.join(FILE_NAME), br#"{"version":999}"#).unwrap();
        let s = Settings::open(&dir).unwrap();
        assert!(!s.collection().enabled);
        assert_eq!(
            fs::read(dir.join("settings.json.corrupt")).unwrap(),
            br#"{"version":999}"#
        );
        assert!(!Settings::open(&dir).unwrap().collection().enabled);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_dir_follows_the_platform() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            config_dir_from(Some(PathBuf::from("/x")), home.clone(), false),
            Some(PathBuf::from("/x/construct"))
        );
        assert_eq!(
            config_dir_from(None, home.clone(), false),
            Some(PathBuf::from("/home/u/.config/construct"))
        );
        assert_eq!(
            config_dir_from(Some(PathBuf::from("/x")), home, true),
            Some(PathBuf::from(
                "/home/u/Library/Application Support/construct"
            ))
        );
        assert_eq!(config_dir_from(None, None, false), None);
    }
}
