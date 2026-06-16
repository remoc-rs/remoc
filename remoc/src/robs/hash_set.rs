//! Observable hash set.
//!
//! This provides a locally and remotely observable hash set.
//! The observable hash set sends a change event each time a change is performed on it.
//! The [resulting event stream](HashSetSubscription) can either be processed event-wise
//! or used to build a [mirrored hash set](MirroredHashSet).
//!
//! Changes are sent using a [remote broadcast channel](crate::rch::broadcast), thus
//! subscribers cannot block the observed hash set and are shed when their event buffer
//! exceeds a configurable size.
//!
//! # Basic use
//!
//! Create a [ObservableHashSet] and obtain a [subscription](HashSetSubscription) to it using
//! [ObservableHashSet::subscribe].
//! Send this subscription to a remote endpoint via a [remote channel](crate::rch) and call
//! [HashSetSubscription::mirror] on the remote endpoint to obtain a live mirror of the observed
//! hash set or process each change event individually using [HashSetSubscription::recv].
//!

use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fmt, hash::Hash, mem::take, ops::Deref, sync::Arc};
use tokio::sync::{RwLock, RwLockReadGuard, oneshot, watch};
use tracing::Instrument;

use super::{ChangeNotifier, ChangeSender, RecvError, SendError, default_on_err, send_event};
use crate::{exec, prelude::*};

/// A hash set change event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashSetEvent<T> {
    /// An item was inserted or modified.
    Set(T),
    /// An item was removed.
    Remove(T),
    /// All items were removed.
    Clear,
    /// Shrink capacity to fit.
    ShrinkToFit,
    /// The hash set has reached its final state and
    /// no further events will occur.
    Done,

    // NOTE: All variants with #[serde(skip)] must be at the end of the enum
    //       to workaround serde issue serde-rs/serde#2224.
    //
    /// The incremental subscription has reached the value of the observed
    /// hash set at the time it was subscribed.
    #[serde(skip)]
    InitialComplete,
}

/// A hash set that emits an event for each change.
///
/// Use [subscribe](Self::subscribe) to obtain an event stream
/// that can be used for building a mirror of this hash set.
pub struct ObservableHashSet<T, Codec = crate::codec::Default> {
    hs: HashSet<T>,
    tx: rch::broadcast::Sender<HashSetEvent<T>, Codec>,
    change: ChangeSender,
    on_err: Arc<dyn Fn(SendError) + Send + Sync>,
    done: bool,
}

impl<T, Codec> fmt::Debug for ObservableHashSet<T, Codec>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.hs.fmt(f)
    }
}

impl<T, Codec> From<HashSet<T>> for ObservableHashSet<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn from(hs: HashSet<T>) -> Self {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        Self { hs, tx, change: ChangeSender::new(), on_err: Arc::new(default_on_err), done: false }
    }
}

impl<T, Codec> From<ObservableHashSet<T, Codec>> for HashSet<T> {
    fn from(ohs: ObservableHashSet<T, Codec>) -> Self {
        ohs.hs
    }
}

impl<T, Codec> Default for ObservableHashSet<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn default() -> Self {
        Self::from(HashSet::new())
    }
}

impl<T, Codec> ObservableHashSet<T, Codec>
where
    T: Eq + Hash + Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Creates an empty observable hash set.
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

    /// Subscribes to change events from this observable hash set.
    ///
    /// The current contents of the hash set is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe(&self, buffer: usize) -> HashSetSubscription<T, Codec> {
        HashSetSubscription::new(
            HashSetInitialValue::new_value(self.hs.clone()),
            if self.done { None } else { Some(self.tx.subscribe(buffer)) },
        )
    }

    /// Subscribes to change events from this observable hash set with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the hash set are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe_incremental(&self, buffer: usize) -> HashSetSubscription<T, Codec> {
        HashSetSubscription::new(
            HashSetInitialValue::new_incremental(self.hs.clone(), self.on_err.clone()),
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

    /// Adds a value to the set.
    ///
    /// A [HashSetEvent::Set] change event is sent.
    ///
    /// Returns whether the set did have this value present.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn insert(&mut self, value: T) -> bool {
        self.assert_not_done();
        self.change.notify();

        send_event(&self.tx, &*self.on_err, HashSetEvent::Set(value.clone()));
        self.hs.insert(value)
    }

    /// Adds a value to the set, replacing the existing value, if any.
    ///
    /// A [HashSetEvent::Set] change event is sent.
    ///
    /// Returns the replaced value, if any.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.    
    pub fn replace(&mut self, value: T) -> Option<T> {
        self.assert_not_done();
        self.change.notify();

        send_event(&self.tx, &*self.on_err, HashSetEvent::Set(value.clone()));
        self.hs.replace(value)
    }

    /// Removes the value under the specified key.
    ///
    /// A [HashSetEvent::Remove] change event is sent.
    ///
    /// Returns whether the set did have this value present.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: std::borrow::Borrow<Q>,
        Q: Hash + Eq,
    {
        self.assert_not_done();

        match self.hs.take(value) {
            Some(v) => {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, HashSetEvent::Remove(v));
                true
            }
            None => false,
        }
    }

    /// Removes the value under the specified key.
    ///
    /// A [HashSetEvent::Remove] change event is sent.
    ///
    /// Returns the removed value, if any.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn take<Q>(&mut self, value: &Q) -> Option<T>
    where
        T: std::borrow::Borrow<Q>,
        Q: Hash + Eq,
    {
        self.assert_not_done();

        match self.hs.take(value) {
            Some(v) => {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, HashSetEvent::Remove(v.clone()));
                Some(v)
            }
            None => None,
        }
    }

    /// Removes all items.
    ///
    /// A [HashSetEvent::Clear] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn clear(&mut self) {
        self.assert_not_done();

        if !self.hs.is_empty() {
            self.hs.clear();
            self.change.notify();
            send_event(&self.tx, &*self.on_err, HashSetEvent::Clear);
        }
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// A [HashSetEvent::Remove] change event is sent for every element that is removed.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.assert_not_done();

        self.hs.retain(|v| {
            if f(v) {
                true
            } else {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, HashSetEvent::Remove(v.clone()));
                false
            }
        });
    }

    /// Shrinks the capacity of the hash set as much as possible.
    ///
    /// A [HashSetEvent::ShrinkToFit] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn shrink_to_fit(&mut self) {
        self.assert_not_done();
        send_event(&self.tx, &*self.on_err, HashSetEvent::ShrinkToFit);
        self.hs.shrink_to_fit()
    }

    /// Panics when `done` has been called.
    fn assert_not_done(&self) {
        if self.done {
            panic!("observable hash set cannot be changed after done has been called");
        }
    }

    /// Prevents further changes of this hash set and notifies
    /// are subscribers that no further events will occur.
    ///
    /// Methods that modify the hash set will panic after this has been called.
    /// It is still possible to subscribe to this observable hash set.
    pub fn done(&mut self) {
        if !self.done {
            send_event(&self.tx, &*self.on_err, HashSetEvent::Done);
            self.done = true;
        }
    }

    /// Returns `true` if [done](Self::done) has been called and further
    /// changes are prohibited.
    ///
    /// Methods that modify the hash set will panic in this case.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Extracts the underlying hash set.
    ///
    /// If [done](Self::done) has not been called before this method,
    /// subscribers will receive an error.
    pub fn into_inner(self) -> HashSet<T> {
        self.into()
    }
}

impl<T, Codec> Deref for ObservableHashSet<T, Codec> {
    type Target = HashSet<T>;

    fn deref(&self) -> &Self::Target {
        &self.hs
    }
}

impl<T, Codec> Extend<T> for ObservableHashSet<T, Codec>
where
    T: RemoteSend + Eq + Hash + Clone,
    Codec: crate::codec::Codec,
{
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.insert(value);
        }
    }
}

struct MirroredHashSetInner<T> {
    hs: HashSet<T>,
    complete: bool,
    done: bool,
    error: Option<RecvError>,
    max_size: usize,
}

impl<T> MirroredHashSetInner<T>
where
    T: Eq + Hash,
{
    fn handle_event(&mut self, event: HashSetEvent<T>) -> Result<(), RecvError> {
        match event {
            HashSetEvent::InitialComplete => {
                self.complete = true;
            }
            HashSetEvent::Set(v) => {
                self.hs.insert(v);
                if self.hs.len() > self.max_size {
                    return Err(RecvError::MaxSizeExceeded(self.max_size));
                }
            }
            HashSetEvent::Remove(k) => {
                self.hs.remove(&k);
            }
            HashSetEvent::Clear => {
                self.hs.clear();
            }
            HashSetEvent::ShrinkToFit => {
                self.hs.shrink_to_fit();
            }
            HashSetEvent::Done => {
                self.done = true;
            }
        }
        Ok(())
    }
}

/// Initial value of an observable hash set subscription.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend + Eq + Hash, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend + Eq + Hash, Codec: crate::codec::Codec"))]
enum HashSetInitialValue<T, Codec = crate::codec::Default> {
    /// Initial value is present.
    Value(HashSet<T>),
    /// Initial value is received incrementally.
    Incremental {
        /// Number of elements.
        len: usize,
        /// Receiver.
        rx: rch::mpsc::Receiver<T, Codec>,
    },
}

impl<T, Codec> HashSetInitialValue<T, Codec>
where
    T: RemoteSend + Eq + Hash + Clone,
    Codec: crate::codec::Codec,
{
    /// Transmits the initial value as a whole.
    fn new_value(hs: HashSet<T>) -> Self {
        Self::Value(hs)
    }

    /// Transmits the initial value incrementally.
    fn new_incremental(hs: HashSet<T>, on_err: Arc<dyn Fn(SendError) + Send + Sync>) -> Self {
        let (tx, rx) = rch::mpsc::channel(128);
        let len = hs.len();

        exec::spawn(
            async move {
                for v in hs.into_iter() {
                    match tx.send(v).await {
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

/// Observable hash set subscription.
///
/// This can be sent to a remote endpoint via a [remote channel](crate::rch).
/// Then, on the remote endpoint, [mirror](Self::mirror) can be used to build
/// and keep up-to-date a mirror of the observed hash set.
///
/// The event stream can also be processed event-wise using [recv](Self::recv).
/// If the subscription is not incremental [take_initial](Self::take_initial) must
/// be called before the first call to [recv](Self::recv).
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend + Eq + Hash, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend + Eq + Hash, Codec: crate::codec::Codec"))]
pub struct HashSetSubscription<T, Codec = crate::codec::Default> {
    /// Value of hash set at time of subscription.
    initial: HashSetInitialValue<T, Codec>,
    /// Initial value received completely.
    #[serde(skip, default)]
    complete: bool,
    /// Change events receiver.
    ///
    /// `None` if [ObservableHashSet::done] has been called before subscribing.
    events: Option<rch::broadcast::Receiver<HashSetEvent<T>, Codec>>,
    /// Event stream ended.
    #[serde(skip, default)]
    done: bool,
}

impl<T, Codec> HashSetSubscription<T, Codec>
where
    T: RemoteSend + Eq + Hash + Clone,
    Codec: crate::codec::Codec,
{
    fn new(
        initial: HashSetInitialValue<T, Codec>, events: Option<rch::broadcast::Receiver<HashSetEvent<T>, Codec>>,
    ) -> Self {
        Self { initial, complete: false, events, done: false }
    }

    /// Returns whether the subscription is incremental.
    pub fn is_incremental(&self) -> bool {
        matches!(self.initial, HashSetInitialValue::Incremental { .. })
    }

    /// Returns whether the initial value event or
    /// stream of events that build up the initial value
    /// has completed or [take_initial](Self::take_initial) has been called.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Returns whether the observed hash set has indicated that no further
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
    pub fn take_initial(&mut self) -> Option<HashSet<T>> {
        match &mut self.initial {
            HashSetInitialValue::Value(value) if !self.complete => {
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
    pub async fn recv(&mut self) -> Result<Option<HashSetEvent<T>>, RecvError> {
        // Provide initial value events.
        if !self.complete {
            match &mut self.initial {
                HashSetInitialValue::Incremental { len, rx } => {
                    if *len > 0 {
                        match rx.recv().await? {
                            Some(v) => {
                                // Provide incremental initial value event.
                                *len -= 1;
                                return Ok(Some(HashSetEvent::Set(v)));
                            }
                            None => return Err(RecvError::Closed),
                        }
                    } else {
                        // Provide incremental initial value complete event.
                        self.complete = true;
                        return Ok(Some(HashSetEvent::InitialComplete));
                    }
                }
                HashSetInitialValue::Value(_) => {
                    panic!("take_initial must be called before recv for non-incremental subscription");
                }
            }
        }

        // Provide change event.
        if let Some(rx) = &mut self.events {
            match rx.recv().await? {
                HashSetEvent::Done => self.events = None,
                evt => return Ok(Some(evt)),
            }
        }

        // Provide done event.
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            Ok(Some(HashSetEvent::Done))
        }
    }
}

impl<T, Codec> HashSetSubscription<T, Codec>
where
    T: RemoteSend + Eq + Hash + Clone + RemoteSend + Sync,
    Codec: crate::codec::Codec,
{
    /// Mirror the hash set that this subscription is observing.
    ///
    /// `max_size` specifies the maximum allowed size of the mirrored collection.
    /// If this size is reached, processing of events is stopped and
    /// [RecvError::MaxSizeExceeded] is returned.
    pub fn mirror(mut self, max_size: usize) -> MirroredHashSet<T, Codec> {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        let (changed_tx, changed_rx) = watch::channel(());
        let (dropped_tx, mut dropped_rx) = oneshot::channel();

        // Build initial state.
        let inner = Arc::new(RwLock::new(Some(MirroredHashSetInner {
            hs: self.take_initial().unwrap_or_default(),
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

        MirroredHashSet { inner, tx: tx.downgrade(), changed_rx, _dropped_tx: dropped_tx }
    }
}

/// A hash set that is mirroring an observable hash set.
pub struct MirroredHashSet<T, Codec = crate::codec::Default> {
    inner: Arc<RwLock<Option<MirroredHashSetInner<T>>>>,
    tx: rch::broadcast::WeakSender<HashSetEvent<T>, Codec>,
    changed_rx: watch::Receiver<()>,
    _dropped_tx: oneshot::Sender<()>,
}

impl<T, Codec> fmt::Debug for MirroredHashSet<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MirroredHashSet").finish()
    }
}

impl<T, Codec> MirroredHashSet<T, Codec>
where
    T: RemoteSend + Eq + Hash + Clone,
    Codec: crate::codec::Codec,
{
    /// Returns a reference to the current value of the hash set.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed hash set has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableHashSet::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow(&self) -> Result<MirroredHashSetRef<'_, T>, RecvError> {
        let inner = self.inner.read().await;
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredHashSetRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Returns a reference to the current value of the hash set and marks it as seen.
    ///
    /// Thus [changed](Self::changed) will not return immediately until the value changes
    /// after this method returns.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed hash set has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableHashSet::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow_and_update(&mut self) -> Result<MirroredHashSetRef<'_, T>, RecvError> {
        let inner = self.inner.read().await;
        self.changed_rx.borrow_and_update();
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredHashSetRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Stops updating the hash set and returns its current contents.
    pub async fn detach(self) -> HashSet<T> {
        let mut inner = self.inner.write().await;
        inner.take().unwrap().hs
    }

    /// Waits for a change and marks the newest value as seen.
    ///
    /// This also returns when connection to the observed hash set has been lost
    /// or the hash set has been marked as done.
    pub async fn changed(&mut self) {
        let _ = self.changed_rx.changed().await;
    }

    /// Subscribes to change events from this mirrored hash set.
    ///
    /// The current contents of the hash set is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe(&self, buffer: usize) -> Result<HashSetSubscription<T, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(HashSetSubscription::new(HashSetInitialValue::new_value(initial), events))
    }

    /// Subscribes to change events from this mirrored hash set with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the hash set are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].    
    pub async fn subscribe_incremental(&self, buffer: usize) -> Result<HashSetSubscription<T, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(HashSetSubscription::new(
            HashSetInitialValue::new_incremental(initial, Arc::new(default_on_err)),
            events,
        ))
    }
}

impl<T, Codec> Drop for MirroredHashSet<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// A snapshot view of an observable hash set.
pub struct MirroredHashSetRef<'a, T>(RwLockReadGuard<'a, MirroredHashSetInner<T>>);

impl<T> MirroredHashSetRef<'_, T> {
    /// Returns `true` if the initial state of an incremental subscription has
    /// been reached.
    pub fn is_complete(&self) -> bool {
        self.0.complete
    }

    /// Returns `true` if the observed hash set has been marked as done by calling
    /// [ObservableHashSet::done] and thus no further changes can occur.
    pub fn is_done(&self) -> bool {
        self.0.done
    }
}

impl<T> fmt::Debug for MirroredHashSetRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.hs.fmt(f)
    }
}

impl<T> Deref for MirroredHashSetRef<'_, T> {
    type Target = HashSet<T>;

    fn deref(&self) -> &Self::Target {
        &self.0.hs
    }
}

impl<T> Drop for MirroredHashSetRef<'_, T> {
    fn drop(&mut self) {
        // required for drop order
    }
}
