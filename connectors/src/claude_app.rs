//! The Claude app keeps its chats and Cowork sessions only in the `IndexedDB` cache of its web
//! view: the database `claude-conversation-store`, object store `trees`, one record per
//! conversation (a Cowork session's key starts with `cowork:`). The app holds the files open and
//! writes them while it runs, so a reader copies them with [`copy_claude_app_cache`] and reads
//! the copy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::indexeddb::{self, Error, Files, StoredValue};
use crate::leveldb;

/// The origin whose `IndexedDB` holds the cache.
const ORIGIN: &str = "https_claude.ai_0";
const DATABASE: &str = "claude-conversation-store";
const STORE: &str = "trees";
/// How many times a copy is tried when the database moves on while it is copied.
const COPY_ATTEMPTS: usize = 3;

/// The cache of the Claude app for `home`: the `IndexedDB` directory of its web view, on macOS.
#[must_use]
pub fn claude_app_cache(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Claude")
        .join("IndexedDB")
}

pub(crate) fn leveldb_dir(cache: &Path) -> PathBuf {
    cache.join(format!("{ORIGIN}.indexeddb.leveldb"))
}

pub(crate) fn blob_dir(cache: &Path) -> PathBuf {
    cache.join(format!("{ORIGIN}.indexeddb.blob"))
}

/// Copies the state of the cache into `into`: `CURRENT`, the manifest it names, the files that
/// manifest says are live, and the blob files. What is in `into` from an earlier copy is
/// replaced. When the manifest changed while the files were copied (the app flushed or compacted
/// meanwhile), or a file was gone before it was copied, the copy starts again.
///
/// # Errors
/// Fails when the cache has no database of the app, when `into` is the cache, inside it or
/// holds it, when the copy keeps being overtaken, or when a file cannot be copied.
pub fn copy_claude_app_cache(cache: &Path, into: &Path) -> Result<(), Error> {
    let uncopyable = |e: io::Error| Error::Uncopyable(into.to_owned(), e.to_string());
    let source =
        fs::canonicalize(cache).map_err(|e| Error::Unreadable(cache.to_owned(), e.to_string()))?;
    // Checked before anything is made at `into`, which may not be there yet.
    let target = resolved(into).map_err(uncopyable)?;
    if target.starts_with(&source) || source.starts_with(&target) {
        return Err(Error::Uncopyable(
            into.to_owned(),
            "the copy would be inside the cache, or hold it".into(),
        ));
    }
    let mut attempt = 1;
    loop {
        match copy_once(cache, into)? {
            Copied::All => return Ok(()),
            Copied::Overtaken(error) if attempt >= COPY_ATTEMPTS => return Err(error),
            Copied::Overtaken(_) => attempt += 1,
        }
    }
}

/// The real path `path` stands for, also when it is not there yet: the real path of its nearest
/// folder that is there, with the rest of `path` after it.
fn resolved(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut rest = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.exists() {
        let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
            break;
        };
        rest.push(name.to_owned());
        existing = parent;
    }
    let mut real = fs::canonicalize(existing)?;
    for name in rest.iter().rev() {
        real.push(name);
    }
    Ok(real)
}

/// How one copy went.
enum Copied {
    All,
    /// The database moved on while it was copied: a file was gone, or the manifest changed.
    Overtaken(Error),
}

fn copy_once(cache: &Path, into: &Path) -> Result<Copied, Error> {
    let (leveldb, to_leveldb) = (leveldb_dir(cache), leveldb_dir(into));
    let (blobs, to_blobs) = (blob_dir(cache), blob_dir(into));
    // `CURRENT` can name a manifest that LevelDB replaces before it is read, as when the app
    // opens the database.
    let manifest = match leveldb::current_manifest(&leveldb) {
        Ok(manifest) => manifest,
        Err(error) => return Ok(Copied::Overtaken(error)),
    };
    for dir in [&to_leveldb, &to_blobs] {
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(Error::Uncopyable(dir.clone(), e.to_string()));
            }
            _ => {}
        }
    }
    let unwritable = |path: &Path, e: io::Error| Error::Uncopyable(path.to_owned(), e.to_string());
    // The copy holds the person's conversations: its folder is theirs alone.
    private_dir(into).map_err(|e| unwritable(into, e))?;
    fs::create_dir_all(&to_leveldb).map_err(|e| unwritable(&to_leveldb, e))?;
    let current = to_leveldb.join("CURRENT");
    fs::write(&current, format!("{}\n", manifest.name)).map_err(|e| unwritable(&current, e))?;
    let manifest_copy = to_leveldb.join(&manifest.name);
    fs::write(&manifest_copy, &manifest.bytes).map_err(|e| unwritable(&manifest_copy, e))?;
    // The blob files are copied before the records and again after them. A value written
    // meanwhile has its new file in the second copy; an old file that the app removes after it
    // wrote a newer value is still in the first, for the records that were copied before.
    if let Copied::Overtaken(error) = copy_blobs(&blobs, &to_blobs)? {
        return Ok(Copied::Overtaken(error));
    }
    for path in leveldb::live_files(&leveldb, &manifest.version)? {
        let Some(name) = path.file_name() else {
            continue;
        };
        if let Copied::Overtaken(error) = copy_file(&path, &to_leveldb.join(name))? {
            return Ok(Copied::Overtaken(error));
        }
    }
    if let Copied::Overtaken(error) = copy_blobs(&blobs, &to_blobs)? {
        return Ok(Copied::Overtaken(error));
    }
    // A flush or a compaction while the files were copied changes the manifest, and the copy
    // may then hold logs of the new state next to the manifest of the old one.
    let overtaken = |reason: String| Copied::Overtaken(Error::Unreadable(leveldb.clone(), reason));
    Ok(match leveldb::current_manifest(&leveldb) {
        Ok(now) if now.name == manifest.name && now.bytes == manifest.bytes => Copied::All,
        Ok(_) => overtaken("the manifest changed while the cache was copied".into()),
        Err(error) => Copied::Overtaken(error),
    })
}

/// Makes `dir`, and makes it readable by its owner only.
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn copy_blobs(blobs: &Path, to_blobs: &Path) -> Result<Copied, Error> {
    if blobs.is_dir() {
        copy_tree(blobs, to_blobs)
    } else {
        Ok(Copied::All)
    }
}

fn copy_file(from: &Path, to: &Path) -> Result<Copied, Error> {
    match fs::copy(from, to) {
        Ok(_) => Ok(Copied::All),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Copied::Overtaken(Error::Unreadable(
            from.to_owned(),
            e.to_string(),
        ))),
        Err(e) => Err(Error::Uncopyable(
            to.to_owned(),
            format!("from {}: {e}", from.display()),
        )),
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<Copied, Error> {
    let unreadable = |path: &Path, e: io::Error| Error::Unreadable(path.to_owned(), e.to_string());
    fs::create_dir_all(to).map_err(|e| Error::Uncopyable(to.to_owned(), e.to_string()))?;
    for entry in fs::read_dir(from).map_err(|e| unreadable(from, e))? {
        let path = entry.map_err(|e| unreadable(from, e))?.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        let copied = if path.is_dir() {
            copy_tree(&path, &to.join(name))?
        } else {
            copy_file(&path, &to.join(name))?
        };
        if let Copied::Overtaken(error) = copied {
            return Ok(Copied::Overtaken(error));
        }
    }
    Ok(Copied::All)
}

/// The newest value of every conversation in the cache at `cache`, keyed by conversation and
/// sorted by key.
///
/// # Errors
/// Fails when the database directory or one of its record files cannot be read, and when it
/// holds no conversation store of the app.
pub fn read_claude_app_cache(cache: &Path) -> Result<Vec<StoredValue>, indexeddb::Error> {
    let (leveldb, blobs) = (leveldb_dir(cache), blob_dir(cache));
    let files = Files {
        leveldb: &leveldb,
        blobs: &blobs,
    };
    indexeddb::read_store(&files, DATABASE, STORE)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::indexeddb::{Error, ValueError};

    type Read = Vec<(String, Result<Value, ValueError>)>;

    const CHAT: &str = "c0000000-0000-4000-8000-000000000001";
    const HUMAN: &str = "m0000000-0000-4000-8000-000000000001";
    const ASSISTANT: &str = "m0000000-0000-4000-8000-000000000002";

    /// The cache that `tests/fixtures/claude-app/write-cache.js` wrote with Electron.
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-app/IndexedDB")
    }

    /// A copy of the fixture made fresh for the test `name`.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("claude-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        copy_tree(&fixture(), &dir).unwrap();
        dir
    }

    fn read(cache: &Path) -> Read {
        read_claude_app_cache(cache)
            .unwrap()
            .into_iter()
            .map(|c| (c.key, c.value))
            .collect()
    }

    fn keys(read: &Read) -> Vec<&str> {
        read.iter().map(|(key, _)| key.as_str()).collect()
    }

    fn value<'a>(read: &'a Read, key: &str) -> &'a Value {
        let (_, value) = read.iter().find(|(k, _)| k == key).unwrap();
        value.as_ref().unwrap()
    }

    #[test]
    fn every_conversation_is_read_at_its_newest_version_and_a_deleted_one_is_left_out() {
        let read = read(&fixture());
        assert_eq!(
            keys(&read),
            [
                CHAT,
                "cowork:cse_fixture_long",
                "cowork:cse_fixture_medium",
                "cowork:cse_fixture_short",
            ]
        );
        // The second version of the record, with both events.
        let short = &value(&read, "cowork:cse_fixture_short")["tree"]["events"];
        assert_eq!(short.as_array().unwrap().len(), 2);
        assert_eq!(
            short[1]["payload"]["message"],
            json!({ "role": "assistant", "content": "Готово: файл создан." })
        );
    }

    #[test]
    fn a_chat_reads_with_its_texts_and_the_same_message_in_both_of_its_lists() {
        let read = read(&fixture());
        let tree = &value(&read, CHAT)["tree"];
        assert_eq!(tree["current_leaf_message_uuid"], ASSISTANT);
        assert_eq!(
            tree["messageByUuid"][HUMAN]["content"][0]["text"],
            "Расскажи мне про Берлин"
        );
        assert_eq!(
            tree["messageByUuid"][ASSISTANT]["content"],
            json!([
                { "type": "thinking", "thinking": "The person asks about Berlin." },
                { "type": "text", "text": "Берлин — столица Германии." },
            ])
        );
        // The list holds the same objects as the map: written once, then referred to.
        assert_eq!(tree["chat_messages"][1], tree["messageByUuid"][ASSISTANT]);
    }

    #[test]
    fn a_session_kept_compressed_or_in_a_blob_file_reads_whole() {
        let read = read(&fixture());
        let medium = &value(&read, "cowork:cse_fixture_medium")["tree"];
        assert_eq!(medium["events"].as_array().unwrap().len(), 40);
        assert_eq!(
            medium["events"][39]["payload"]["message"]["content"],
            "повтор ".repeat(120)
        );
        let long = &value(&read, "cowork:cse_fixture_long")["tree"];
        assert_eq!(long["events"].as_array().unwrap().len(), 120);
        assert_eq!(
            (long["floorSeq"].clone(), long["headSeq"].clone()),
            (json!(7), json!(126))
        );
        assert_eq!(long["hasOlder"], true);
        assert_eq!(long["events"][119]["dedupKey"], "d0000126");
    }

    #[test]
    fn a_missing_blob_file_fails_only_its_own_conversation() {
        let cache = scratch("no-blob");
        fs::remove_file(blob_dir(&cache).join("1/00/2")).unwrap();
        let read = read(&cache);
        let _ = fs::remove_dir_all(&cache);
        let (_, long) = read
            .iter()
            .find(|(key, _)| key == "cowork:cse_fixture_long")
            .unwrap();
        assert!(matches!(
            long,
            Err(ValueError::BlobFile(path, _)) if path.ends_with("1/00/2")
        ));
        assert_eq!(read.iter().filter(|(_, value)| value.is_ok()).count(), 3);
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_copy_holds_the_state_the_manifest_names_and_replaces_an_earlier_copy() {
        let cache = scratch("source");
        let source = leveldb_dir(&cache);
        fs::write(source.join("LOCK"), "").unwrap();
        fs::write(source.join("LOG"), "Creating DB /home/someone").unwrap();
        // A table still being written: the manifest does not name it.
        fs::write(source.join("000009.ldb"), b"half a table").unwrap();
        let copy = std::env::temp_dir().join(format!("claude-app-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&copy);
        fs::create_dir_all(leveldb_dir(&copy)).unwrap();
        fs::write(
            leveldb_dir(&copy).join("000001.ldb"),
            b"left by an earlier copy",
        )
        .unwrap();
        copy_claude_app_cache(&cache, &copy).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&copy).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "the copy is for its owner only");
        }
        let (copied, blobs) = (
            names(&leveldb_dir(&copy)),
            names(&blob_dir(&copy).join("1/00")),
        );
        let read = keys(&read(&copy)).len();
        let _ = fs::remove_dir_all(&cache);
        let _ = fs::remove_dir_all(&copy);
        assert_eq!(copied, ["000003.log", "CURRENT", "MANIFEST-000001"]);
        assert_eq!(blobs, ["2"]);
        assert_eq!(read, 4);
    }

    /// Every file and folder under `dir`, as paths relative to it, sorted.
    fn tree(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            found.push(path.strip_prefix(dir).unwrap().to_owned());
            if path.is_dir() {
                let below = tree(&path);
                found.extend(
                    below
                        .into_iter()
                        .map(|p| path.strip_prefix(dir).unwrap().join(p)),
                );
            }
        }
        found.sort();
        found
    }

    /// A copy onto the cache through a symbolic link to it, where the system has them.
    #[cfg(unix)]
    fn copy_through_a_link(cache: &Path) -> Vec<Result<(), Error>> {
        let link = std::env::temp_dir().join(format!("claude-app-link-{}", std::process::id()));
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(cache, &link).unwrap();
        let attempt = copy_claude_app_cache(cache, &link);
        let _ = fs::remove_file(&link);
        vec![attempt]
    }

    #[cfg(not(unix))]
    fn copy_through_a_link(_cache: &Path) -> Vec<Result<(), Error>> {
        Vec::new()
    }

    #[test]
    fn a_copy_onto_the_cache_by_any_path_is_refused_and_leaves_the_cache_whole() {
        let cache = scratch("onto");
        let before = tree(&cache);
        let roundabout = cache.join("..").join(cache.file_name().unwrap());
        let attempts: Vec<Result<(), Error>> = [
            copy_claude_app_cache(&cache, &cache),
            copy_claude_app_cache(&cache, &roundabout),
            copy_claude_app_cache(&cache, &leveldb_dir(&cache)),
            // Not there yet: nothing may be made for it inside the cache.
            copy_claude_app_cache(&cache, &leveldb_dir(&cache).join("copy/deeper")),
            // A folder that holds the cache.
            copy_claude_app_cache(&cache, cache.parent().unwrap()),
        ]
        .into_iter()
        .chain(copy_through_a_link(&cache))
        .collect();
        let after = tree(&cache);
        let _ = fs::remove_dir_all(&cache);
        assert!(
            attempts
                .iter()
                .all(|a| matches!(a, Err(Error::Uncopyable(..)))),
            "{attempts:?}"
        );
        assert_eq!(after, before);
    }

    #[test]
    fn a_cache_that_is_not_there_has_no_database_or_store_or_lacks_a_live_table_is_an_error() {
        let missing = std::env::temp_dir().join(format!("claude-app-none-{}", std::process::id()));
        assert!(matches!(
            read_claude_app_cache(&missing),
            Err(Error::Unreadable(path, _)) if path.ends_with("CURRENT")
        ));
        assert!(copy_claude_app_cache(&missing, &missing.join("copy")).is_err());
        assert!(!missing.exists());

        let no_database = scratch("no-database");
        cut_log(&no_database, 0);
        let read_no_database = read_claude_app_cache(&no_database);
        let no_store = scratch("no-store");
        cut_log(&no_store, BEFORE_STORE);
        let read_no_store = read_claude_app_cache(&no_store);
        let store_unfinished = scratch("store-unfinished");
        cut_log(&store_unfinished, IN_STORE_CREATION);
        let read_store_unfinished = read_claude_app_cache(&store_unfinished);
        let _ = fs::remove_dir_all(&no_database);
        let _ = fs::remove_dir_all(&no_store);
        let _ = fs::remove_dir_all(&store_unfinished);
        assert!(matches!(read_no_database, Err(Error::NoDatabase(name)) if name == DATABASE));
        assert!(matches!(read_no_store, Err(Error::NoStore(name)) if name == STORE));
        // The store was written, in a transaction that is undone.
        assert!(matches!(read_store_unfinished, Err(Error::NoStore(name)) if name == STORE));

        // A manifest that names a table the directory does not have.
        let lacking = scratch("lacking");
        let dir = leveldb_dir(&lacking);
        let edit = crate::leveldb::tests::edit(3, &[9], &[]);
        fs::write(
            dir.join("MANIFEST-000010"),
            crate::leveldb::tests::log(&[edit]),
        )
        .unwrap();
        fs::write(dir.join("CURRENT"), "MANIFEST-000010\n").unwrap();
        let read_lacking = read_claude_app_cache(&lacking);
        let _ = fs::remove_dir_all(&lacking);
        assert!(matches!(
            read_lacking,
            Err(Error::Unreadable(path, _)) if path.ends_with("000009.ldb")
        ));
    }

    #[test]
    fn a_log_older_than_the_manifest_says_is_not_read() {
        let cache = scratch("old-log");
        let dir = leveldb_dir(&cache);
        let edit = crate::leveldb::tests::edit(3, &[], &[]);
        fs::write(
            dir.join("MANIFEST-000010"),
            crate::leveldb::tests::log(&[edit]),
        )
        .unwrap();
        fs::write(dir.join("CURRENT"), "MANIFEST-000010\n").unwrap();
        // Log 2 deletes the chat at a sequence number past every other one: read, it would hide it.
        let mut key = vec![0, 1, 1, 1, 1, 36];
        key.extend(CHAT.encode_utf16().flat_map(u16::to_be_bytes));
        let mut batch = u64::MAX.to_le_bytes()[..7].to_vec();
        batch.push(0);
        batch.extend(1_u32.to_le_bytes());
        batch.extend([0, u8::try_from(key.len()).unwrap()]);
        batch.extend(&key);
        fs::write(dir.join("000002.log"), crate::leveldb::tests::log(&[batch])).unwrap();
        let read = read(&cache);
        let _ = fs::remove_dir_all(&cache);
        assert_eq!(keys(&read).first(), Some(&CHAT));
    }

    /// Lengths of the fixture's log: after the database is named and before the transaction
    /// that creates the store; inside that transaction, after it wrote the store; inside the
    /// third transaction of `write-cache.js` (the long and the medium session), after it wrote
    /// the long one; and after that transaction committed, before its undo log is cleaned up.
    /// A new fixture needs these lengths found again.
    const BEFORE_STORE: u64 = 216;
    const IN_STORE_CREATION: u64 = 1_000;
    const IN_THIRD: u64 = 12_000;
    const COMMITTED_THIRD: u64 = 14_800;

    /// Cuts the log of the copy `cache` at `len` bytes, as a copy taken while the app writes.
    fn cut_log(cache: &Path, len: u64) {
        let log = fs::OpenOptions::new()
            .write(true)
            .open(leveldb_dir(cache).join("000003.log"))
            .unwrap();
        log.set_len(len).unwrap();
    }

    #[test]
    fn a_transaction_the_copy_caught_unfinished_is_undone_and_a_committed_one_is_kept() {
        let unfinished = scratch("unfinished");
        cut_log(&unfinished, IN_THIRD);
        let read_unfinished = read(&unfinished);
        let committed = scratch("committed");
        cut_log(&committed, COMMITTED_THIRD);
        let read_committed = read(&committed);
        let _ = fs::remove_dir_all(&unfinished);
        let _ = fs::remove_dir_all(&committed);

        // The state before the third transaction: no long or medium session, no half of one.
        assert_eq!(keys(&read_unfinished), [CHAT, "cowork:cse_fixture_short"]);
        assert!(read_unfinished.iter().all(|(_, value)| value.is_ok()));
        assert_eq!(keys(&read_committed).len(), 4);
        assert!(read_committed.iter().all(|(_, value)| value.is_ok()));
    }

    #[test]
    fn the_cache_is_under_the_apps_folder_on_macos() {
        assert_eq!(
            claude_app_cache(Path::new("/Users/p")),
            Path::new("/Users/p/Library/Application Support/Claude/IndexedDB")
        );
    }
}
