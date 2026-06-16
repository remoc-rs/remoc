//! Observable vector.
//!
//! This provides a locally and remotely observable vector.
//! The observable vector sends a change event each time a change is performed on it.
//! The [resulting event stream](VecSubscription) can either be processed event-wise
//! or used to build a [mirrored vector](MirroredVec).
//!
//! Changes are sent using a [remote broadcast channel](crate::rch::broadcast), thus
//! subscribers cannot block the observed vector and are shed when their event buffer
//! exceeds a configurable size.
//!
//! # Alternatives
//!
//! The [observable append-only list](super::list) is more memory-efficient as it does not
//! require a send buffer for each subscriber.
//! You should prefer it, if you are only appending to the vector.
//!
//! # Basic use
//!
//! Create a [ObservableVec] and obtain a [subscription](VecSubscription) to it using
//! [ObservableVec::subscribe].
//! Send this subscription to a remote endpoint via a [remote channel](crate::rch) and call
//! [VecSubscription::mirror] on the remote endpoint to obtain a live mirror of the observed
//! vector or process each change event individually using [VecSubscription::recv].
//!

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt,
    iter::{Enumerate, FusedIterator},
    mem::take,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::sync::{RwLock, RwLockReadGuard, oneshot, watch};
use tracing::Instrument;

use super::{ChangeNotifier, ChangeSender, RecvError, SendError, default_on_err, send_event};
use crate::{exec, prelude::*};

/// A vector change event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VecEvent<T> {
    /// An item was added at the end.
    Push(T),
    /// The last item was removed.
    Pop,
    /// An item was inserted at the given index.
    Insert(usize, T),
    /// The specified item was modified.
    Set(usize, T),
    /// The specified item was removed.
    Remove(usize),
    /// The specified element was removed and replaced by the last element.
    SwapRemove(usize),
    /// All vector elements have been set to the specified value.
    Fill(T),
    /// The vector has been resized to the specified length.
    Resize(usize, T),
    /// The vector has been truncated to the specified length.
    Truncate(usize),
    /// Retain the specified elements.
    Retain(HashSet<usize>),
    /// Retain the inverse of the specified elements.
    RetainNot(HashSet<usize>),
    /// All items were removed.
    Clear,
    /// Shrink capacity to fit.
    ShrinkToFit,
    /// The vector has reached its final state and
    /// no further events will occur.
    Done,

    // NOTE: All variants with #[serde(skip)] must be at the end of the enum
    //       to workaround serde issue serde-rs/serde#2224.
    //
    /// The incremental subscription has reached the value of the observed
    /// vector at the time it was subscribed.
    #[serde(skip)]
    InitialComplete,
}

/// A vector that emits an event for each change.
///
/// Use [subscribe](Self::subscribe) to obtain an event stream
/// that can be used for building a mirror of this vector.
///
/// Due to current limitations of Rust you must use [get_mut](Self::get_mut)
/// to obtain a mutable reference to an element instead of using
/// the indexing notation.
/// Also, mutable slicing is not supported and updates must be done
/// element-wise.
pub struct ObservableVec<T, Codec = crate::codec::Default> {
    v: Vec<T>,
    tx: rch::broadcast::Sender<VecEvent<T>, Codec>,
    change: ChangeSender,
    on_err: Arc<dyn Fn(SendError) + Send + Sync>,
    done: bool,
}

impl<T, Codec> fmt::Debug for ObservableVec<T, Codec>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.v.fmt(f)
    }
}

impl<T, Codec> From<Vec<T>> for ObservableVec<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn from(hs: Vec<T>) -> Self {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        Self { v: hs, tx, change: ChangeSender::new(), on_err: Arc::new(default_on_err), done: false }
    }
}

impl<T, Codec> From<ObservableVec<T, Codec>> for Vec<T> {
    fn from(ohs: ObservableVec<T, Codec>) -> Self {
        ohs.v
    }
}

impl<T, Codec> Default for ObservableVec<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl<T, Codec> ObservableVec<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Creates an empty observable vector.
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

    /// Subscribes to change events from this observable vector.
    ///
    /// The current contents of the vector is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe(&self, buffer: usize) -> VecSubscription<T, Codec> {
        VecSubscription::new(
            VecInitialValue::new_value(self.v.clone()),
            if self.done { None } else { Some(self.tx.subscribe(buffer)) },
        )
    }

    /// Subscribes to change events from this observable vector with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the vector are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe_incremental(&self, buffer: usize) -> VecSubscription<T, Codec> {
        VecSubscription::new(
            VecInitialValue::new_incremental(self.v.clone(), self.on_err.clone()),
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

    /// Appends an element at the end.
    ///
    /// A [VecEvent::Push] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn push(&mut self, value: T) {
        self.assert_not_done();
        self.change.notify();

        send_event(&self.tx, &*self.on_err, VecEvent::Push(value.clone()));
        self.v.push(value);
    }

    /// Removes the last element and returns it, or `None` if the vector is empty.
    ///
    /// A [VecEvent::Pop] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn pop(&mut self) -> Option<T> {
        self.assert_not_done();

        match self.v.pop() {
            Some(value) => {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, VecEvent::Pop);
                Some(value)
            }
            None => None,
        }
    }

    /// Gets a mutable reference to the value at the specified index.
    ///
    /// A [VecEvent::Set] change event is sent if the reference is accessed mutably.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn get_mut(&mut self, index: usize) -> Option<RefMut<'_, T, Codec>> {
        self.assert_not_done();

        match self.v.get_mut(index) {
            Some(value) => Some(RefMut {
                index,
                value,
                changed: false,
                tx: &self.tx,
                change: &self.change,
                on_err: &*self.on_err,
            }),
            None => None,
        }
    }

    /// Mutably iterates over items.
    ///
    /// A [VecEvent::Set] change event is sent for each value that is accessed mutably.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.    
    pub fn iter_mut(&mut self) -> IterMut<'_, T, Codec> {
        self.assert_not_done();

        IterMut {
            inner: self.v.iter_mut().enumerate(),
            tx: &self.tx,
            change: &self.change,
            on_err: &*self.on_err,
        }
    }

    /// Inserts an element at the specified position, shift all elements after it to the right.
    ///
    /// A [VecEvent::Insert] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or `index > len`.
    pub fn insert(&mut self, index: usize, value: T) {
        self.assert_not_done();

        let value_event = value.clone();
        self.v.insert(index, value);
        self.change.notify();
        send_event(&self.tx, &*self.on_err, VecEvent::Insert(index, value_event));
    }

    /// Removes the element at the specified position, shifting all elements after it to the left.
    ///
    /// A [VecEvent::Remove] change event is sent.
    ///
    /// Returns the removed value.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or the index is out of bounds.
    pub fn remove(&mut self, index: usize) -> T {
        self.assert_not_done();

        let value = self.v.remove(index);
        self.change.notify();
        send_event(&self.tx, &*self.on_err, VecEvent::Remove(index));
        value
    }

    /// Removes the element at the specified position, replacing it with the last element.
    ///
    /// A [VecEvent::SwapRemove] change event is sent.
    ///
    /// Returns the removed value.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or the index is out of bounds.
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.assert_not_done();

        let value = self.v.swap_remove(index);
        self.change.notify();
        send_event(&self.tx, &*self.on_err, VecEvent::SwapRemove(index));
        value
    }

    /// Fills the vector with the specified value.
    ///
    /// A [VecEvent::Fill] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn fill(&mut self, value: T) {
        self.assert_not_done();

        self.change.notify();
        send_event(&self.tx, &*self.on_err, VecEvent::Fill(value.clone()));
        self.v.fill(value);
    }

    /// Resizes the vector to the specified length, filling each additional slot with the
    /// specified value.
    ///
    /// A [VecEvent::Resize] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn resize(&mut self, new_len: usize, value: T) {
        self.assert_not_done();

        if new_len != self.v.len() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecEvent::Resize(new_len, value.clone()));
            self.v.resize(new_len, value);
        }
    }

    /// Truncates the vector to the specified length.
    ///
    /// If `new_len` is greater than the current length, this has no effect.
    ///
    /// A [VecEvent::Truncate] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn truncate(&mut self, new_len: usize) {
        self.assert_not_done();

        if new_len < self.len() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecEvent::Truncate(new_len));
            self.v.truncate(new_len);
        }
    }

    /// Removes all items.
    ///
    /// A [VecEvent::Clear] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn clear(&mut self) {
        self.assert_not_done();

        if !self.v.is_empty() {
            self.v.clear();
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecEvent::Clear);
        }
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// A [VecEvent::Retain] or [VecEvent::RetainNot] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.assert_not_done();

        let mut keep = HashSet::new();
        let mut remove = HashSet::new();
        let mut pos = 0;

        self.v.retain(|v| {
            let keep_this = f(v);

            if keep_this {
                keep.insert(pos);
            } else {
                remove.insert(pos);
            }
            pos += 1;

            keep_this
        });

        if !remove.is_empty() {
            self.change.notify();
            if keep.len() < remove.len() {
                send_event(&self.tx, &*self.on_err, VecEvent::Retain(keep));
            } else {
                send_event(&self.tx, &*self.on_err, VecEvent::RetainNot(remove));
            }
        }
    }

    /// Shrinks the capacity of the vector as much as possible.
    ///
    /// A [VecEvent::ShrinkToFit] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn shrink_to_fit(&mut self) {
        self.assert_not_done();
        send_event(&self.tx, &*self.on_err, VecEvent::ShrinkToFit);
        self.v.shrink_to_fit()
    }

    /// Panics when `done` has been called.
    fn assert_not_done(&self) {
        if self.done {
            panic!("observable vector cannot be changed after done has been called");
        }
    }

    /// Prevents further changes of this vector and notifies
    /// are subscribers that no further events will occur.
    ///
    /// Methods that modify the vector will panic after this has been called.
    /// It is still possible to subscribe to this observable vector.
    pub fn done(&mut self) {
        if !self.done {
            send_event(&self.tx, &*self.on_err, VecEvent::Done);
            self.done = true;
        }
    }

    /// Returns `true` if [done](Self::done) has been called and further
    /// changes are prohibited.
    ///
    /// Methods that modify the vector will panic in this case.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Extracts the underlying vector.
    ///
    /// If [done](Self::done) has not been called before this method,
    /// subscribers will receive an error.
    pub fn into_inner(self) -> Vec<T> {
        self.into()
    }
}

impl<T, Codec> Deref for ObservableVec<T, Codec> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

impl<T, Codec> Extend<T> for ObservableVec<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.push(value);
        }
    }
}

/// A mutable reference to a value inside an [observable vector](ObservableVec).
///
/// A [VecEvent::Set] change event is sent when this reference is dropped and the
/// value has been accessed mutably.
pub struct RefMut<'a, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    index: usize,
    value: &'a mut T,
    changed: bool,
    tx: &'a rch::broadcast::Sender<VecEvent<T>, Codec>,
    change: &'a ChangeSender,
    on_err: &'a dyn Fn(SendError),
}

impl<T, Codec> Deref for RefMut<'_, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<T, Codec> DerefMut for RefMut<'_, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.changed = true;
        self.value
    }
}

impl<T, Codec> Drop for RefMut<'_, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn drop(&mut self) {
        if self.changed {
            self.change.notify();
            send_event(self.tx, self.on_err, VecEvent::Set(self.index, self.value.clone()));
        }
    }
}

/// A mutable iterator over the items in an [observable vector](ObservableVec).
///
/// A [VecEvent::Set] change event is sent for each value that is accessed mutably.
pub struct IterMut<'a, T, Codec> {
    inner: Enumerate<std::slice::IterMut<'a, T>>,
    tx: &'a rch::broadcast::Sender<VecEvent<T>, Codec>,
    change: &'a ChangeSender,
    on_err: &'a dyn Fn(SendError),
}

impl<'a, T, Codec> Iterator for IterMut<'a, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    type Item = RefMut<'a, T, Codec>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.next() {
            Some((index, value)) => Some(RefMut {
                index,
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

impl<T, Codec> ExactSizeIterator for IterMut<'_, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T, Codec> DoubleEndedIterator for IterMut<'_, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        match self.inner.next_back() {
            Some((index, value)) => Some(RefMut {
                index,
                value,
                changed: false,
                tx: self.tx,
                change: self.change,
                on_err: self.on_err,
            }),
            None => None,
        }
    }
}

impl<T, Codec> FusedIterator for IterMut<'_, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
}

struct MirroredVecInner<T> {
    v: Vec<T>,
    complete: bool,
    done: bool,
    error: Option<RecvError>,
    max_size: usize,
}

impl<T> MirroredVecInner<T>
where
    T: Clone,
{
    fn handle_event(&mut self, event: VecEvent<T>) -> Result<(), RecvError> {
        match event {
            VecEvent::InitialComplete => {
                self.complete = true;
            }
            VecEvent::Push(v) => {
                self.v.push(v);
                if self.v.len() > self.max_size {
                    return Err(RecvError::MaxSizeExceeded(self.max_size));
                }
            }
            VecEvent::Pop => {
                self.v.pop();
            }
            VecEvent::Insert(i, v) => {
                if i > self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.insert(i, v);
            }
            VecEvent::Set(i, v) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v[i] = v;
            }
            VecEvent::Remove(i) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.remove(i);
            }
            VecEvent::SwapRemove(i) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.swap_remove(i);
            }
            VecEvent::Fill(v) => {
                self.v.fill(v);
            }
            VecEvent::Resize(l, v) => {
                self.v.resize(l, v);
            }
            VecEvent::Truncate(l) => {
                self.v.truncate(l);
            }
            VecEvent::Retain(r) => {
                let mut pos = 0;
                self.v.retain(|_| {
                    let keep_this = r.contains(&pos);
                    pos += 1;
                    keep_this
                });
            }
            VecEvent::RetainNot(nr) => {
                let mut pos = 0;
                self.v.retain(|_| {
                    let keep_this = !nr.contains(&pos);
                    pos += 1;
                    keep_this
                });
            }
            VecEvent::Clear => {
                self.v.clear();
            }
            VecEvent::ShrinkToFit => {
                self.v.shrink_to_fit();
            }
            VecEvent::Done => {
                self.done = true;
            }
        }
        Ok(())
    }
}

/// Initial value of an observable vector subscription.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
enum VecInitialValue<T, Codec = crate::codec::Default> {
    /// Initial value is present.
    Value(Vec<T>),
    /// Initial value is received incrementally.
    Incremental {
        /// Number of elements.
        len: usize,
        /// Receiver.
        rx: rch::mpsc::Receiver<T, Codec>,
    },
}

impl<T, Codec> VecInitialValue<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Transmits the initial value as a whole.
    fn new_value(hs: Vec<T>) -> Self {
        Self::Value(hs)
    }

    /// Transmits the initial value incrementally.
    fn new_incremental(hs: Vec<T>, on_err: Arc<dyn Fn(SendError) + Send + Sync>) -> Self {
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

/// Observable vector subscription.
///
/// This can be sent to a remote endpoint via a [remote channel](crate::rch).
/// Then, on the remote endpoint, [mirror](Self::mirror) can be used to build
/// and keep up-to-date a mirror of the observed vector.
///
/// The event stream can also be processed event-wise using [recv](Self::recv).
/// If the subscription is not incremental [take_initial](Self::take_initial) must
/// be called before the first call to [recv](Self::recv).
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
pub struct VecSubscription<T, Codec = crate::codec::Default> {
    /// Value of vector at time of subscription.
    initial: VecInitialValue<T, Codec>,
    /// Initial value received completely.
    #[serde(skip, default)]
    complete: bool,
    /// Change events receiver.
    ///
    /// `None` if [ObservableVec::done] has been called before subscribing.
    events: Option<rch::broadcast::Receiver<VecEvent<T>, Codec>>,
    /// Event stream ended.
    #[serde(skip, default)]
    done: bool,
}

impl<T, Codec> VecSubscription<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn new(
        initial: VecInitialValue<T, Codec>, events: Option<rch::broadcast::Receiver<VecEvent<T>, Codec>>,
    ) -> Self {
        Self { initial, complete: false, events, done: false }
    }

    /// Returns whether the subscription is incremental.
    pub fn is_incremental(&self) -> bool {
        matches!(self.initial, VecInitialValue::Incremental { .. })
    }

    /// Returns whether the initial value event or
    /// stream of events that build up the initial value
    /// has completed or [take_initial](Self::take_initial) has been called.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Returns whether the observed vector has indicated that no further
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
    pub fn take_initial(&mut self) -> Option<Vec<T>> {
        match &mut self.initial {
            VecInitialValue::Value(value) if !self.complete => {
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
    pub async fn recv(&mut self) -> Result<Option<VecEvent<T>>, RecvError> {
        // Provide initial value events.
        if !self.complete {
            match &mut self.initial {
                VecInitialValue::Incremental { len, rx } => {
                    if *len > 0 {
                        match rx.recv().await? {
                            Some(v) => {
                                // Provide incremental initial value event.
                                *len -= 1;
                                return Ok(Some(VecEvent::Push(v)));
                            }
                            None => return Err(RecvError::Closed),
                        }
                    } else {
                        // Provide incremental initial value complete event.
                        self.complete = true;
                        return Ok(Some(VecEvent::InitialComplete));
                    }
                }
                VecInitialValue::Value(_) => {
                    panic!("take_initial must be called before recv for non-incremental subscription");
                }
            }
        }

        // Provide change event.
        if let Some(rx) = &mut self.events {
            match rx.recv().await? {
                VecEvent::Done => self.events = None,
                evt => return Ok(Some(evt)),
            }
        }

        // Provide done event.
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            Ok(Some(VecEvent::Done))
        }
    }
}

impl<T, Codec> VecSubscription<T, Codec>
where
    T: RemoteSend + Clone + Sync,
    Codec: crate::codec::Codec,
{
    /// Mirror the vector that this subscription is observing.
    ///
    /// `max_size` specifies the maximum allowed size of the mirrored collection.
    /// If this size is reached, processing of events is stopped and
    /// [RecvError::MaxSizeExceeded] is returned.
    pub fn mirror(mut self, max_size: usize) -> MirroredVec<T, Codec> {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        let (changed_tx, changed_rx) = watch::channel(());
        let (dropped_tx, mut dropped_rx) = oneshot::channel();

        // Build initial state.
        let inner = Arc::new(RwLock::new(Some(MirroredVecInner {
            v: self.take_initial().unwrap_or_default(),
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

        MirroredVec { inner, tx: tx.downgrade(), changed_rx, _dropped_tx: dropped_tx }
    }
}

/// A vector that is mirroring an observable vector.
pub struct MirroredVec<T, Codec = crate::codec::Default> {
    inner: Arc<RwLock<Option<MirroredVecInner<T>>>>,
    tx: rch::broadcast::WeakSender<VecEvent<T>, Codec>,
    changed_rx: watch::Receiver<()>,
    _dropped_tx: oneshot::Sender<()>,
}

impl<T, Codec> fmt::Debug for MirroredVec<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MirroredVec").finish()
    }
}

impl<T, Codec> MirroredVec<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Returns a reference to the current value of the vector.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed vector has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableVec::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow(&self) -> Result<MirroredVecRef<'_, T>, RecvError> {
        let inner = self.inner.read().await;
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredVecRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Returns a reference to the current value of the vector and marks it as seen.
    ///
    /// Thus [changed](Self::changed) will not return immediately until the value changes
    /// after this method returns.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed vector has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableVec::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow_and_update(&mut self) -> Result<MirroredVecRef<'_, T>, RecvError> {
        let inner = self.inner.read().await;
        self.changed_rx.borrow_and_update();
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredVecRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Stops updating the vector and returns its current contents.
    pub async fn detach(self) -> Vec<T> {
        let mut inner = self.inner.write().await;
        inner.take().unwrap().v
    }

    /// Waits for a change and marks the newest value as seen.
    ///
    /// This also returns when connection to the observed vector has been lost
    /// or the vector has been marked as done.
    pub async fn changed(&mut self) {
        let _ = self.changed_rx.changed().await;
    }

    /// Subscribes to change events from this mirrored vector.
    ///
    /// The current contents of the vector is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe(&self, buffer: usize) -> Result<VecSubscription<T, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(VecSubscription::new(VecInitialValue::new_value(initial), events))
    }

    /// Subscribes to change events from this mirrored vector with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the vector are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe_incremental(&self, buffer: usize) -> Result<VecSubscription<T, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(VecSubscription::new(VecInitialValue::new_incremental(initial, Arc::new(default_on_err)), events))
    }
}

impl<T, Codec> Drop for MirroredVec<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// A snapshot view of an observable vector.
pub struct MirroredVecRef<'a, T>(RwLockReadGuard<'a, MirroredVecInner<T>>);

impl<T> MirroredVecRef<'_, T> {
    /// Returns `true` if the initial state of an incremental subscription has
    /// been reached.
    pub fn is_complete(&self) -> bool {
        self.0.complete
    }

    /// Returns `true` if the observed vector has been marked as done by calling
    /// [ObservableVec::done] and thus no further changes can occur.
    pub fn is_done(&self) -> bool {
        self.0.done
    }
}

impl<T> fmt::Debug for MirroredVecRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.v.fmt(f)
    }
}

impl<T> Deref for MirroredVecRef<'_, T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0.v
    }
}

impl<T> Drop for MirroredVecRef<'_, T> {
    fn drop(&mut self) {
        // required for drop order
    }
}
