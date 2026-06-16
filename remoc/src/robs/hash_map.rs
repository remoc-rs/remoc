//! Observable hash map.
//!
//! This provides a locally and remotely observable hash map.
//! The observable hash map sends a change event each time a change is performed on it.
//! The [resulting event stream](HashMapSubscription) can either be processed event-wise
//! or used to build a [mirrored hash map](MirroredHashMap).
//!
//! Changes are sent using a [remote broadcast channel](crate::rch::broadcast), thus
//! subscribers cannot block the observed hash map and are shed when their event buffer
//! exceeds a configurable size.
//!
//! # Basic use
//!
//! Create a [ObservableHashMap] and obtain a [subscription](HashMapSubscription) to it using
//! [ObservableHashMap::subscribe].
//! Send this subscription to a remote endpoint via a [remote channel](crate::rch) and call
//! [HashMapSubscription::mirror] on the remote endpoint to obtain a live mirror of the observed
//! hash map or process each change event individually using [HashMapSubscription::recv].
//!

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    iter::FusedIterator,
    mem::take,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::sync::{RwLock, RwLockReadGuard, oneshot, watch};
use tracing::Instrument;

use super::{ChangeNotifier, ChangeSender, RecvError, SendError, default_on_err, send_event};
use crate::{exec, prelude::*};

/// A hash map change event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashMapEvent<K, V> {
    /// An item was inserted or modified.
    Set(K, V),
    /// An item was removed.
    Remove(K),
    /// All items were removed.
    Clear,
    /// Shrink capacity to fit.
    ShrinkToFit,
    /// The hash map has reached its final state and
    /// no further events will occur.
    Done,

    // NOTE: All variants with #[serde(skip)] must be at the end of the enum
    //       to workaround serde issue serde-rs/serde#2224.
    //
    /// The incremental subscription has reached the value of the observed
    /// hash map at the time it was subscribed.
    #[serde(skip)]
    InitialComplete,
}

/// A hash map that emits an event for each change.
///
/// Use [subscribe](Self::subscribe) to obtain an event stream
/// that can be used for building a mirror of this hash map.
pub struct ObservableHashMap<K, V, Codec = crate::codec::Default> {
    hm: HashMap<K, V>,
    tx: rch::broadcast::Sender<HashMapEvent<K, V>, Codec>,
    change: ChangeSender,
    on_err: Arc<dyn Fn(SendError) + Send + Sync>,
    done: bool,
}

impl<K, V, Codec> fmt::Debug for ObservableHashMap<K, V, Codec>
where
    K: fmt::Debug,
    V: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.hm.fmt(f)
    }
}

impl<K, V, Codec> From<HashMap<K, V>> for ObservableHashMap<K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn from(hm: HashMap<K, V>) -> Self {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        Self { hm, tx, on_err: Arc::new(default_on_err), change: ChangeSender::new(), done: false }
    }
}

impl<K, V, Codec> From<ObservableHashMap<K, V, Codec>> for HashMap<K, V> {
    fn from(ohm: ObservableHashMap<K, V, Codec>) -> Self {
        ohm.hm
    }
}

impl<K, V, Codec> Default for ObservableHashMap<K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn default() -> Self {
        Self::from(HashMap::new())
    }
}

impl<K, V, Codec> ObservableHashMap<K, V, Codec>
where
    K: Eq + Hash + Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Creates an empty observable hash map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the error handler function that is called when sending an
    /// event fails.
    pub fn set_error_handler<E>(&mut self, on_err: E)
    where
        E: Fn(SendError) + Send + Sync + 'static,
    {
        self.on_err = Arc::new(on_err);
    }

    /// Subscribes to change events from this observable hash map.
    ///
    /// The current contents of the hash map is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe(&self, buffer: usize) -> HashMapSubscription<K, V, Codec> {
        HashMapSubscription::new(
            HashMapInitialValue::new_value(self.hm.clone()),
            if self.done { None } else { Some(self.tx.subscribe(buffer)) },
        )
    }

    /// Subscribes to change events from this observable hash map with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the hash map are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe_incremental(&self, buffer: usize) -> HashMapSubscription<K, V, Codec> {
        HashMapSubscription::new(
            HashMapInitialValue::new_incremental(self.hm.clone(), self.on_err.clone()),
            if self.done { None } else { Some(self.tx.subscribe(buffer)) },
        )
    }

    /// Current number of subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Returns a [change notifier](ChangeNotifier) that can be used *locally* to be
    /// notified of changes to this collection.
    pub fn notifier(&self) -> ChangeNotifier {
        self.change.subscribe()
    }

    /// Inserts a value under a key.
    ///
    /// A [HashMapEvent::Set] change event is sent.
    ///
    /// Returns the value previously stored under the key, if any.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.assert_not_done();
        self.change.notify();

        send_event(&self.tx, &*self.on_err, HashMapEvent::Set(k.clone(), v.clone()));
        self.hm.insert(k, v)
    }

    /// Removes the value under the specified key.
    ///
    /// A [HashMapEvent::Remove] change event is sent.
    ///
    /// The value is returned.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq,
    {
        self.assert_not_done();

        match self.hm.remove_entry(k) {
            Some((k, v)) => {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, HashMapEvent::Remove(k));
                Some(v)
            }
            None => None,
        }
    }

    /// Removes all items.
    ///
    /// A [HashMapEvent::Clear] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn clear(&mut self) {
        self.assert_not_done();

        if !self.hm.is_empty() {
            self.hm.clear();
            self.change.notify();
            send_event(&self.tx, &*self.on_err, HashMapEvent::Clear);
        }
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// A [HashMapEvent::Remove] change event is sent for every element that is removed.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        self.assert_not_done();

        self.hm.retain(|k, v| {
            if f(k, v) {
                true
            } else {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, HashMapEvent::Remove(k.clone()));
                false
            }
        });
    }

    /// Gets the given key’s corresponding entry in the map for in-place manipulation.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V, Codec> {
        self.assert_not_done();

        match self.hm.entry(key) {
            std::collections::hash_map::Entry::Occupied(inner) => Entry::Occupied(OccupiedEntry {
                inner,
                tx: &self.tx,
                change: &self.change,
                on_err: &*self.on_err,
            }),
            std::collections::hash_map::Entry::Vacant(inner) => {
                Entry::Vacant(VacantEntry { inner, tx: &self.tx, change: &self.change, on_err: &*self.on_err })
            }
        }
    }

    /// Gets a mutable reference to the value under the specified key.
    ///
    /// A [HashMapEvent::Set] change event is sent if the reference is accessed mutably.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<RefMut<'_, K, V, Codec>>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq,
    {
        self.assert_not_done();

        match self.hm.get_key_value(k) {
            Some((key, _)) => {
                let key = key.clone();
                let value = self.hm.get_mut(k).unwrap();
                Some(RefMut {
                    key,
                    value,
                    changed: false,
                    tx: &self.tx,
                    change: &self.change,
                    on_err: &*self.on_err,
                })
            }
            None => None,
        }
    }

    /// Mutably iterates over the key-value pairs.
    ///
    /// A [HashMapEvent::Set] change event is sent for each value that is accessed mutably.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V, Codec> {
        self.assert_not_done();

        IterMut { inner: self.hm.iter_mut(), tx: &self.tx, change: &self.change, on_err: &*self.on_err }
    }

    /// Shrinks the capacity of the hash map as much as possible.
    ///
    /// A [HashMapEvent::ShrinkToFit] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn shrink_to_fit(&mut self) {
        self.assert_not_done();
        send_event(&self.tx, &*self.on_err, HashMapEvent::ShrinkToFit);
        self.hm.shrink_to_fit()
    }

    /// Panics when `done` has been called.
    fn assert_not_done(&self) {
        if self.done {
            panic!("observable hash map cannot be changed after done has been called");
        }
    }

    /// Prevents further changes of this hash map and notifies
    /// are subscribers that no further events will occur.
    ///
    /// Methods that modify the hash map will panic after this has been called.
    /// It is still possible to subscribe to this observable hash map.
    pub fn done(&mut self) {
        if !self.done {
            send_event(&self.tx, &*self.on_err, HashMapEvent::Done);
            self.done = true;
        }
    }

    /// Returns `true` if [done](Self::done) has been called and further
    /// changes are prohibited.
    ///
    /// Methods that modify the hash map will panic in this case.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Extracts the underlying hash map.
    ///
    /// If [done](Self::done) has not been called before this method,
    /// subscribers will receive an error.
    pub fn into_inner(self) -> HashMap<K, V> {
        self.into()
    }
}

impl<K, V, Codec> Deref for ObservableHashMap<K, V, Codec> {
    type Target = HashMap<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.hm
    }
}

impl<K, V, Codec> Extend<(K, V)> for ObservableHashMap<K, V, Codec>
where
    K: Eq + Hash + Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}

/// A mutable reference to a value inside an [observable hash map](ObservableHashMap).
///
/// A [HashMapEvent::Set] change event is sent when this reference is dropped and the
/// value has been accessed mutably.
pub struct RefMut<'a, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    key: K,
    value: &'a mut V,
    changed: bool,
    tx: &'a rch::broadcast::Sender<HashMapEvent<K, V>, Codec>,
    change: &'a ChangeSender,
    on_err: &'a dyn Fn(SendError),
}

impl<K, V, Codec> Deref for RefMut<'_, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    type Target = V;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<K, V, Codec> DerefMut for RefMut<'_, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.changed = true;
        self.value
    }
}

impl<K, V, Codec> Drop for RefMut<'_, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn drop(&mut self) {
        if self.changed {
            self.change.notify();
            send_event(self.tx, self.on_err, HashMapEvent::Set(self.key.clone(), self.value.clone()));
        }
    }
}

/// A mutable iterator over the key-value pairs in an [observable hash map](ObservableHashMap).
///
/// A [HashMapEvent::Set] change event is sent for each value that is accessed mutably.
pub struct IterMut<'a, K, V, Codec = crate::codec::Default> {
    inner: std::collections::hash_map::IterMut<'a, K, V>,
    tx: &'a rch::broadcast::Sender<HashMapEvent<K, V>, Codec>,
    change: &'a ChangeSender,
    on_err: &'a dyn Fn(SendError),
}

impl<'a, K, V, Codec> Iterator for IterMut<'a, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    type Item = RefMut<'a, K, V, Codec>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some((key, value)) => Some(RefMut {
                key: key.clone(),
                value,
                changed: false,
                tx: self.tx,
                change: self.change,
                on_err: self.on_err,
            }),
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V, Codec> ExactSizeIterator for IterMut<'_, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V, Codec> FusedIterator for IterMut<'_, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
}

/// A view into a single entry in an observable hash map, which may either be
/// vacant or occupied.
///
/// This is returned by [ObservableHashMap::entry].
#[derive(Debug)]
pub enum Entry<'a, K, V, Codec = crate::codec::Default> {
    /// An occupied entry.
    Occupied(OccupiedEntry<'a, K, V, Codec>),
    /// A vacant entry.
    Vacant(VacantEntry<'a, K, V, Codec>),
}

impl<'a, K, V, Codec> Entry<'a, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Ensures a value is in the entry by inserting the default if empty,
    /// and returns a mutable reference to the value in the entry.
    pub fn or_insert(self, default: V) -> RefMut<'a, K, V, Codec> {
        match self {
            Self::Occupied(ocu) => ocu.into_mut(),
            Self::Vacant(vac) => vac.insert(default),
        }
    }

    /// Ensures a value is in the entry by inserting the result of the default
    /// function if empty, and returns a mutable reference to the value in the entry.
    pub fn or_insert_with<F: FnOnce() -> V>(self, default: F) -> RefMut<'a, K, V, Codec> {
        match self {
            Self::Occupied(ocu) => ocu.into_mut(),
            Self::Vacant(vac) => vac.insert(default()),
        }
    }

    /// Ensures a value is in the entry by inserting the result of the default
    /// function with key as argument if empty, and returns a mutable reference to the value in the entry.
    pub fn or_insert_with_key<F: FnOnce(&K) -> V>(self, default: F) -> RefMut<'a, K, V, Codec> {
        match self {
            Self::Occupied(ocu) => ocu.into_mut(),
            Self::Vacant(vac) => {
                let value = default(vac.key());
                vac.insert(value)
            }
        }
    }

    /// Returns a reference to this entry’s key.
    pub fn key(&self) -> &K {
        match self {
            Self::Occupied(ocu) => ocu.key(),
            Self::Vacant(vac) => vac.key(),
        }
    }

    /// Provides in-place mutable access to an occupied entry before any potential inserts into the map.
    pub fn and_modify<F: FnOnce(&mut V)>(mut self, f: F) -> Self {
        if let Self::Occupied(ocu) = &mut self {
            let mut value = ocu.get_mut();
            f(&mut *value);
        }
        self
    }
}

impl<'a, K, V, Codec> Entry<'a, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend + Default,
    Codec: crate::codec::Codec,
{
    /// Ensures a value is in the entry by inserting the default value if empty,
    /// and returns a mutable reference to the value in the entry.
    pub fn or_default(self) -> RefMut<'a, K, V, Codec> {
        #[allow(clippy::unwrap_or_default)]
        self.or_insert_with(V::default)
    }
}

/// A view into an occupied entry in an observable hash map.
pub struct OccupiedEntry<'a, K, V, Codec = crate::codec::Default> {
    inner: std::collections::hash_map::OccupiedEntry<'a, K, V>,
    tx: &'a rch::broadcast::Sender<HashMapEvent<K, V>, Codec>,
    change: &'a ChangeSender,
    on_err: &'a dyn Fn(SendError),
}

impl<K, V, Codec> fmt::Debug for OccupiedEntry<'_, K, V, Codec>
where
    K: fmt::Debug,
    V: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'a, K, V, Codec> OccupiedEntry<'a, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Gets a reference to the key in the entry.
    pub fn key(&self) -> &K {
        self.inner.key()
    }

    /// Take the ownership of the key and value from the map.
    ///
    /// A [HashMapEvent::Remove] change event is sent.
    pub fn remove_entry(self) -> (K, V) {
        let (k, v) = self.inner.remove_entry();
        self.change.notify();
        send_event(self.tx, self.on_err, HashMapEvent::Remove(k.clone()));
        (k, v)
    }

    /// Gets a reference to the value in the entry.
    pub fn get(&self) -> &V {
        self.inner.get()
    }

    /// Gets a mutable reference to the value in the entry.
    pub fn get_mut(&mut self) -> RefMut<'_, K, V, Codec> {
        RefMut {
            key: self.inner.key().clone(),
            value: self.inner.get_mut(),
            changed: false,
            tx: self.tx,
            change: self.change,
            on_err: self.on_err,
        }
    }

    /// Converts this into a mutable reference to the value in
    /// the entry with a lifetime bound to the map itself.
    pub fn into_mut(self) -> RefMut<'a, K, V, Codec> {
        let key = self.inner.key().clone();
        RefMut {
            key,
            value: self.inner.into_mut(),
            changed: false,
            tx: self.tx,
            change: self.change,
            on_err: self.on_err,
        }
    }

    /// Sets the value of the entry, and returns the entry’s old value.
    ///
    /// A [HashMapEvent::Set] change event is sent.
    pub fn insert(&mut self, value: V) -> V {
        self.change.notify();
        send_event(self.tx, self.on_err, HashMapEvent::Set(self.inner.key().clone(), value.clone()));
        self.inner.insert(value)
    }

    /// Takes the value out of the entry, and returns it.
    ///
    /// A [HashMapEvent::Remove] change event is sent.
    pub fn remove(self) -> V {
        let (k, v) = self.inner.remove_entry();
        self.change.notify();
        send_event(self.tx, self.on_err, HashMapEvent::Remove(k));
        v
    }
}

/// A view into a vacant entry in an observable hash map.
pub struct VacantEntry<'a, K, V, Codec = crate::codec::Default> {
    inner: std::collections::hash_map::VacantEntry<'a, K, V>,
    tx: &'a rch::broadcast::Sender<HashMapEvent<K, V>, Codec>,
    change: &'a ChangeSender,
    on_err: &'a dyn Fn(SendError),
}

impl<K, V, Codec> fmt::Debug for VacantEntry<'_, K, V, Codec>
where
    K: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'a, K, V, Codec> VacantEntry<'a, K, V, Codec>
where
    K: Clone + RemoteSend,
    V: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Gets a reference to the key.
    pub fn key(&self) -> &K {
        self.inner.key()
    }

    /// Take ownership of the key.
    pub fn into_key(self) -> K {
        self.inner.into_key()
    }

    /// Sets the value of the entry, and returns a mutable reference to it.
    ///
    /// A [HashMapEvent::Set] change event is sent.
    pub fn insert(self, value: V) -> RefMut<'a, K, V, Codec> {
        let key = self.inner.key().clone();
        self.change.notify();
        send_event(self.tx, self.on_err, HashMapEvent::Set(key.clone(), value.clone()));
        let value = self.inner.insert(value);
        RefMut { key, value, changed: false, tx: self.tx, change: self.change, on_err: self.on_err }
    }
}

struct MirroredHashMapInner<K, V> {
    hm: HashMap<K, V>,
    complete: bool,
    done: bool,
    error: Option<RecvError>,
    max_size: usize,
}

impl<K, V> MirroredHashMapInner<K, V>
where
    K: Eq + Hash,
{
    fn handle_event(&mut self, event: HashMapEvent<K, V>) -> Result<(), RecvError> {
        match event {
            HashMapEvent::InitialComplete => {
                self.complete = true;
            }
            HashMapEvent::Set(k, v) => {
                self.hm.insert(k, v);
                if self.hm.len() > self.max_size {
                    return Err(RecvError::MaxSizeExceeded(self.max_size));
                }
            }
            HashMapEvent::Remove(k) => {
                self.hm.remove(&k);
            }
            HashMapEvent::Clear => {
                self.hm.clear();
            }
            HashMapEvent::ShrinkToFit => {
                self.hm.shrink_to_fit();
            }
            HashMapEvent::Done => {
                self.done = true;
            }
        }
        Ok(())
    }
}

/// Initial value of an observable hash map subscription.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "K: RemoteSend + Eq + Hash, V: RemoteSend, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "K: RemoteSend + Eq + Hash, V: RemoteSend, Codec: crate::codec::Codec"))]
enum HashMapInitialValue<K, V, Codec = crate::codec::Default> {
    /// Initial value is present.
    Value(HashMap<K, V>),
    /// Initial value is received incrementally.
    Incremental {
        /// Number of elements.
        len: usize,
        /// Receiver.
        rx: rch::mpsc::Receiver<(K, V), Codec>,
    },
}

impl<K, V, Codec> HashMapInitialValue<K, V, Codec>
where
    K: RemoteSend + Eq + Hash + Clone,
    V: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Transmits the initial value as a whole.
    fn new_value(hm: HashMap<K, V>) -> Self {
        Self::Value(hm)
    }

    /// Transmits the initial value incrementally.
    fn new_incremental(hm: HashMap<K, V>, on_err: Arc<dyn Fn(SendError) + Send + Sync>) -> Self {
        let (tx, rx) = rch::mpsc::channel(128);
        let len = hm.len();

        exec::spawn(
            async move {
                for (k, v) in hm.into_iter() {
                    match tx.send((k, v)).await {
                        Ok(_) => (),
                        Err(err) if err.is_disconnected() => break,
                        Err(err) => match err.try_into() {
                            Ok(err) => (on_err)(err),
                            Err(_) => unreachable!(),
                        },
                    }
                }
            }
            .in_current_span(),
        );

        Self::Incremental { len, rx }
    }
}

/// Observable hash map subscription.
///
/// This can be sent to a remote endpoint via a [remote channel](crate::rch).
/// Then, on the remote endpoint, [mirror](Self::mirror) can be used to build
/// and keep up-to-date a mirror of the observed hash map.
///
/// The event stream can also be processed event-wise using [recv](Self::recv).
/// If the subscription is not incremental [take_initial](Self::take_initial) must
/// be called before the first call to [recv](Self::recv).
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "K: RemoteSend + Eq + Hash, V: RemoteSend, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "K: RemoteSend + Eq + Hash, V: RemoteSend, Codec: crate::codec::Codec"))]
pub struct HashMapSubscription<K, V, Codec = crate::codec::Default> {
    /// Value of hash map at time of subscription.
    initial: HashMapInitialValue<K, V, Codec>,
    /// Initial value received completely.
    #[serde(skip, default)]
    complete: bool,
    /// Change events receiver.
    ///
    /// `None` if [ObservableHashMap::done] has been called before subscribing.
    events: Option<rch::broadcast::Receiver<HashMapEvent<K, V>, Codec>>,
    /// Event stream ended.
    #[serde(skip, default)]
    done: bool,
}

impl<K, V, Codec> HashMapSubscription<K, V, Codec>
where
    K: RemoteSend + Eq + Hash + Clone,
    V: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn new(
        initial: HashMapInitialValue<K, V, Codec>,
        events: Option<rch::broadcast::Receiver<HashMapEvent<K, V>, Codec>>,
    ) -> Self {
        Self { initial, complete: false, events, done: false }
    }

    /// Returns whether the subscription is incremental.
    pub fn is_incremental(&self) -> bool {
        matches!(self.initial, HashMapInitialValue::Incremental { .. })
    }

    /// Returns whether the initial value event or
    /// stream of events that build up the initial value
    /// has completed or [take_initial](Self::take_initial) has been called.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Returns whether the observed hash map has indicated that no further
    /// change events will occur.
    pub fn is_done(&self) -> bool {
        self.events.is_none() || self.done
    }

    /// Take the initial value.
    ///
    /// This is only possible if the subscription is not incremental
    /// and the initial value has not already been taken.
    /// Otherwise `None` is returned.
    ///
    /// If the subscription is not incremental this must be called before the
    /// first call to [recv](Self::recv).
    pub fn take_initial(&mut self) -> Option<HashMap<K, V>> {
        match &mut self.initial {
            HashMapInitialValue::Value(value) if !self.complete => {
                self.complete = true;
                Some(take(value))
            }
            _ => None,
        }
    }

    /// Receives the next change event.
    ///
    /// # Panics
    /// Panics when the subscription is not incremental and [take_initial](Self::take_initial)
    /// has not been called.
    pub async fn recv(&mut self) -> Result<Option<HashMapEvent<K, V>>, RecvError> {
        // Provide initial value events.
        if !self.complete {
            match &mut self.initial {
                HashMapInitialValue::Incremental { len, rx } => {
                    if *len > 0 {
                        match rx.recv().await? {
                            Some((k, v)) => {
                                // Provide incremental initial value event.
                                *len -= 1;
                                return Ok(Some(HashMapEvent::Set(k, v)));
                            }
                            None => return Err(RecvError::Closed),
                        }
                    } else {
                        // Provide incremental initial value complete event.
                        self.complete = true;
                        return Ok(Some(HashMapEvent::InitialComplete));
                    }
                }
                HashMapInitialValue::Value(_) => {
                    panic!("take_initial must be called before recv for non-incremental subscription");
                }
            }
        }

        // Provide change event.
        if let Some(rx) = &mut self.events {
            match rx.recv().await? {
                HashMapEvent::Done => self.events = None,
                evt => return Ok(Some(evt)),
            }
        }

        // Provide done event.
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            Ok(Some(HashMapEvent::Done))
        }
    }
}

impl<K, V, Codec> HashMapSubscription<K, V, Codec>
where
    K: RemoteSend + Eq + Hash + Clone + Sync,
    V: RemoteSend + Clone + Sync,
    Codec: crate::codec::Codec,
{
    /// Mirror the hash map that this subscription is observing.
    ///
    /// `max_size` specifies the maximum allowed size of the mirrored collection.
    /// If this size is reached, processing of events is stopped and
    /// [RecvError::MaxSizeExceeded] is returned.
    pub fn mirror(mut self, max_size: usize) -> MirroredHashMap<K, V, Codec> {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        let (changed_tx, changed_rx) = watch::channel(());
        let (dropped_tx, mut dropped_rx) = oneshot::channel();

        // Build initial state.
        let inner = Arc::new(RwLock::new(Some(MirroredHashMapInner {
            hm: self.take_initial().unwrap_or_default(),
            complete: self.is_complete(),
            done: self.is_done(),
            error: None,
            max_size,
        })));
        let inner_task = inner.clone();

        // Process change events.
        let tx_send = tx.clone();
        exec::spawn(
            async move {
                loop {
                    let event = tokio::select! {
                        event = self.recv() => event,
                        _ = &mut dropped_rx => return,
                    };

                    let mut inner = inner_task.write().await;
                    let inner = match inner.as_mut() {
                        Some(inner) => inner,
                        None => return,
                    };

                    changed_tx.send_replace(());

                    match event {
                        Ok(Some(event)) => {
                            if tx_send.receiver_count() > 0 {
                                let _ = tx_send.send(event.clone());
                            }

                            if let Err(err) = inner.handle_event(event) {
                                inner.error = Some(err);
                                return;
                            }

                            if inner.done {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            inner.error = Some(err);
                            return;
                        }
                    }
                }
            }
            .in_current_span(),
        );

        MirroredHashMap { inner, tx: tx.downgrade(), changed_rx, _dropped_tx: dropped_tx }
    }
}

/// A hash map that is mirroring an observable hash map.
pub struct MirroredHashMap<K, V, Codec = crate::codec::Default> {
    inner: Arc<RwLock<Option<MirroredHashMapInner<K, V>>>>,
    tx: rch::broadcast::WeakSender<HashMapEvent<K, V>, Codec>,
    changed_rx: watch::Receiver<()>,
    _dropped_tx: oneshot::Sender<()>,
}

impl<K, V, Codec> fmt::Debug for MirroredHashMap<K, V, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MirroredHashMap").finish()
    }
}

impl<K, V, Codec> MirroredHashMap<K, V, Codec>
where
    K: RemoteSend + Eq + Hash + Clone,
    V: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Returns a reference to the current value of the hash map.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed hash map has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableHashMap::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow(&self) -> Result<MirroredHashMapRef<'_, K, V>, RecvError> {
        let inner = self.inner.read().await;
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredHashMapRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Returns a reference to the current value of the hash map and marks it as seen.
    ///
    /// Thus [changed](Self::changed) will not return immediately until the value changes
    /// after this method returns.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed hash map has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableHashMap::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow_and_update(&mut self) -> Result<MirroredHashMapRef<'_, K, V>, RecvError> {
        let inner = self.inner.read().await;
        self.changed_rx.borrow_and_update();
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredHashMapRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Stops updating the hash map and returns its current contents.
    pub async fn detach(self) -> HashMap<K, V> {
        let mut inner = self.inner.write().await;
        inner.take().unwrap().hm
    }

    /// Waits for a change and marks the newest value as seen.
    ///
    /// This also returns when connection to the observed hash map has been lost
    /// or the hash map has been marked as done.
    pub async fn changed(&mut self) {
        let _ = self.changed_rx.changed().await;
    }

    /// Subscribes to change events from this mirrored hash map.
    ///
    /// The current contents of the hash map is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe(&self, buffer: usize) -> Result<HashMapSubscription<K, V, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(HashMapSubscription::new(HashMapInitialValue::new_value(initial), events))
    }

    /// Subscribes to change events from this mirrored hash map with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the hash map are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe_incremental(
        &self, buffer: usize,
    ) -> Result<HashMapSubscription<K, V, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(HashMapSubscription::new(
            HashMapInitialValue::new_incremental(initial, Arc::new(default_on_err)),
            events,
        ))
    }
}

impl<K, V, Codec> Drop for MirroredHashMap<K, V, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// A snapshot view of an observable hash map.
pub struct MirroredHashMapRef<'a, K, V>(RwLockReadGuard<'a, MirroredHashMapInner<K, V>>);

impl<K, V> MirroredHashMapRef<'_, K, V> {
    /// Returns `true` if the initial state of an incremental subscription has
    /// been reached.
    pub fn is_complete(&self) -> bool {
        self.0.complete
    }

    /// Returns `true` if the observed hash map has been marked as done by calling
    /// [ObservableHashMap::done] and thus no further changes can occur.
    pub fn is_done(&self) -> bool {
        self.0.done
    }
}

impl<K, V> fmt::Debug for MirroredHashMapRef<'_, K, V>
where
    K: fmt::Debug,
    V: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.hm.fmt(f)
    }
}

impl<K, V> Deref for MirroredHashMapRef<'_, K, V> {
    type Target = HashMap<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.0.hm
    }
}

impl<K, V> Drop for MirroredHashMapRef<'_, K, V> {
    fn drop(&mut self) {
        // required for drop order
    }
}
