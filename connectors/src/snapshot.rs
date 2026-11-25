use crate::Record;

/// One record of a [`Snapshot`], with an id that no other record of its session has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identified {
    pub id: String,
    pub record: Record,
}

/// One conversation of a [`Snapshot`]: its session and its records, or why they cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub struct Conversation {
    pub session: String,
    pub records: Result<Vec<Identified>, String>,
}

/// A source that keeps its sessions in a store it rewrites, not in logs it appends to: each read
/// gives every conversation the store holds at that time, and a reader tells the records it has
/// seen by their ids.
pub trait Snapshot: Send {
    /// Short stable name; it is the `tool` of every record and a directory name on disk.
    fn name(&self) -> &'static str;

    /// Whether the app that keeps the store is installed for `home`.
    fn installed(&self, home: &std::path::Path) -> bool;

    /// Every conversation of the store under `home`, or `None` when the store has not changed
    /// since the last read. A store that is not there has no conversations.
    ///
    /// # Errors
    /// The store is there and cannot be read.
    fn read(&mut self, home: &std::path::Path) -> Result<Option<Vec<Conversation>>, String>;

    /// Lets the next read give every conversation, changed or not.
    fn forget(&mut self);
}
