//! Arbitrary data storage.

use std::{
    any::{Any, TypeId},
    collections::{HashMap, hash_map::Entry},
    fmt,
    sync::Arc,
};
use uuid::Uuid;

use super::{cfg::Cfg, msg::ExchangedCfg};

/// Box containing any value that is Send, Sync and static.
pub type AnyBox = Box<dyn Any + Send + Sync + 'static>;

/// An entry in [AnyStorage].
pub type AnyEntry = Arc<tokio::sync::RwLock<Option<AnyBox>>>;

type AnyMap = HashMap<Uuid, AnyEntry>;

type ValueMap = HashMap<TypeId, AnyBox>;

/// Stores arbitrary data of a channel multiplexer connection.
///
/// Entries are indexed by automatically generated keys, while values are
/// indexed by their type.
///
/// Clones share the underlying storage.
///
/// Each endpoint of a connection has its own storage and no data is transferred
/// between them, i.e. storing data does not make it available on the remote
/// endpoint.
#[derive(Clone)]
pub struct AnyStorage {
    cfg: Arc<Cfg>,
    remote_cfg: Arc<ExchangedCfg>,
    entries: Arc<std::sync::Mutex<AnyMap>>,
    values: Arc<std::sync::Mutex<ValueMap>>,
}

impl fmt::Debug for AnyStorage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let entries = self.entries.lock().unwrap();
        let values = self.values.lock().unwrap();
        f.debug_struct("AnyStorage").field("entries", &*entries).field("values", &values.len()).finish()
    }
}

impl AnyStorage {
    /// Creates a new storage.
    pub(crate) fn new(cfg: Arc<Cfg>, remote_cfg: Arc<ExchangedCfg>) -> Self {
        Self {
            cfg,
            remote_cfg,
            entries: Arc::new(std::sync::Mutex::new(AnyMap::new())),
            values: Arc::new(std::sync::Mutex::new(ValueMap::new())),
        }
    }

    /// Configuration of the channel multiplexer this storage belongs to.
    pub fn cfg(&self) -> &Cfg {
        &self.cfg
    }

    /// Remote configuration of the channel multiplexer.
    pub(crate) fn remote_cfg(&self) -> &ExchangedCfg {
        &self.remote_cfg
    }

    /// Insert a new entry into the storage and return its key.
    pub fn insert_entry(&self, entry: AnyEntry) -> Uuid {
        let mut entries = self.entries.lock().unwrap();
        loop {
            let key = Uuid::new_v4();
            if let Entry::Vacant(e) = entries.entry(key) {
                e.insert(entry);
                return key;
            }
        }
    }

    /// Returns the entry from the storage for the specified key.
    pub fn get_entry(&self, key: Uuid) -> Option<AnyEntry> {
        let entries = self.entries.lock().unwrap();
        entries.get(&key).cloned()
    }

    /// Removes the entry for the specified key from the storage and returns it.
    pub fn remove_entry(&self, key: Uuid) -> Option<AnyEntry> {
        let mut entries = self.entries.lock().unwrap();
        entries.remove(&key)
    }

    /// Inserts the value of type `T`, replacing and returning the previously
    /// stored value of that type.
    ///
    /// Since values are indexed by their type, a newtype should be used to
    /// avoid conflicting with other users of the storage.
    pub fn insert<T>(&self, value: T) -> Option<T>
    where
        T: Any + Send + Sync,
    {
        let mut values = self.values.lock().unwrap();
        values.insert(TypeId::of::<T>(), Box::new(value)).map(|value| *value.downcast::<T>().unwrap())
    }

    /// Returns a clone of the stored value of type `T`.
    pub fn get<T>(&self) -> Option<T>
    where
        T: Any + Send + Sync + Clone,
    {
        self.with(T::clone)
    }

    /// Calls the provided function with the stored value of type `T` and
    /// returns its result.
    ///
    /// `None` is returned if no value of that type is stored.
    ///
    /// # Deadlocks
    ///
    /// The storage is locked while the function is executed, thus it must not
    /// access the values of this storage.
    pub fn with<T, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R>
    where
        T: Any + Send + Sync,
    {
        let values = self.values.lock().unwrap();
        values.get(&TypeId::of::<T>()).map(|value| f(value.downcast_ref::<T>().unwrap()))
    }

    /// Removes the stored value of type `T` and returns it.
    pub fn remove<T>(&self) -> Option<T>
    where
        T: Any + Send + Sync,
    {
        let mut values = self.values.lock().unwrap();
        values.remove(&TypeId::of::<T>()).map(|value| *value.downcast::<T>().unwrap())
    }
}
