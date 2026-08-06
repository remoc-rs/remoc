//! Arbitrary data storage.

use std::{
    any::Any,
    collections::{HashMap, hash_map::Entry},
    fmt,
    sync::Arc,
};
use uuid::Uuid;

use super::cfg::Cfg;

/// Box containing any value that is Send, Sync and static.
pub type AnyBox = Box<dyn Any + Send + Sync + 'static>;

/// An entry in [AnyStorage].
pub type AnyEntry = Arc<tokio::sync::RwLock<Option<AnyBox>>>;

type AnyMap = HashMap<Uuid, AnyEntry>;

/// Stores arbitrary data indexed by automatically generated keys.
///
/// Clones share the underlying storage.
#[derive(Clone)]
pub struct AnyStorage {
    cfg: Arc<Cfg>,
    entries: Arc<std::sync::Mutex<AnyMap>>,
}

impl fmt::Debug for AnyStorage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let entries = self.entries.lock().unwrap();
        write!(f, "{:?}", *entries)
    }
}

impl AnyStorage {
    /// Creates a new storage.
    pub(crate) fn new(cfg: Arc<Cfg>) -> Self {
        Self { cfg, entries: Arc::new(std::sync::Mutex::new(AnyMap::new())) }
    }

    /// Configuration of the channel multiplexer this storage belongs to.
    pub fn cfg(&self) -> &Cfg {
        &self.cfg
    }

    /// Insert a new entry into the storage and return its key.
    pub fn insert(&self, entry: AnyEntry) -> Uuid {
        let mut entries = self.entries.lock().unwrap();
        loop {
            let key = Uuid::new_v4();
            if let Entry::Vacant(e) = entries.entry(key) {
                e.insert(entry);
                return key;
            }
        }
    }

    /// Returns the value from the storage for the specified key.
    pub fn get(&self, key: Uuid) -> Option<AnyEntry> {
        let entries = self.entries.lock().unwrap();
        entries.get(&key).cloned()
    }

    /// Removes the value for the specified key from the storage and returns it.
    pub fn remove(&self, key: Uuid) -> Option<AnyEntry> {
        let mut entries = self.entries.lock().unwrap();
        entries.remove(&key)
    }
}
