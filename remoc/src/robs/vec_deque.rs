//! Observable double-ended queue.
//!
//! This provides a locally and remotely observable double-ended queue (VecDeque).
//! The observable VecDeque sends a change event each time a change is performed on it.
//! The [resulting event stream](VecDequeSubscription) can either be processed event-wise
//! or used to build a [mirrored VecDeque](MirroredVecDeque).
//!
//! Changes are sent using a [remote broadcast channel](crate::rch::broadcast), thus
//! subscribers cannot block the observed VecDeque and are shed when their event buffer
//! exceeds a configurable size.
//!
//! # Alternatives
//!
//! The [observable append-only list](super::list) is more memory-efficient as it does not
//! require a send buffer for each subscriber.
//! You should prefer it, if you are only appending to the VecDeque.
//!
//! The [observable vector](super::vec) provides similar functionality but without
//! efficient front operations.
//!
//! # Basic use
//!
//! Create a [ObservableVecDeque] and obtain a [subscription](VecDequeSubscription) to it using
//! [ObservableVecDeque::subscribe].
//! Send this subscription to a remote endpoint via a [remote channel](crate::rch) and call
//! [VecDequeSubscription::mirror] on the remote endpoint to obtain a live mirror of the observed
//! VecDeque or process each change event individually using [VecDequeSubscription::recv].
//!

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
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

/// A VecDeque change event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VecDequeEvent<T> {
    /// An item was added at the end.
    PushBack(T),
    /// An item was added at the front.
    PushFront(T),
    /// The last item was removed.
    PopBack,
    /// The first item was removed.
    PopFront,
    /// An item was inserted at the given index.
    Insert(usize, T),
    /// The specified item was modified.
    Set(usize, T),
    /// The specified item was removed.
    Remove(usize),
    /// The specified element was removed and replaced by the last element.
    SwapRemoveBack(usize),
    /// The specified element was removed and replaced by the first element.
    SwapRemoveFront(usize),
    /// The VecDeque has been resized to the specified length.
    Resize(usize, T),
    /// The VecDeque has been truncated to the specified length.
    Truncate(usize),
    /// Retain the specified elements.
    Retain(HashSet<usize>),
    /// Retain the inverse of the specified elements.
    RetainNot(HashSet<usize>),
    /// All items were removed.
    Clear,
    /// Shrink capacity to fit.
    ShrinkToFit,
    /// The VecDeque has reached its final state and
    /// no further events will occur.
    Done,

    // NOTE: All variants with #[serde(skip)] must be at the end of the enum
    //       to workaround serde issue serde-rs/serde#2224.
    //
    /// The incremental subscription has reached the value of the observed
    /// VecDeque at the time it was subscribed.
    #[serde(skip)]
    InitialComplete,
}

/// A VecDeque that emits an event for each change.
///
/// Use [subscribe](Self::subscribe) to obtain an event stream
/// that can be used for building a mirror of this VecDeque.
///
/// Due to current limitations of Rust you must use [get_mut](Self::get_mut)
/// to obtain a mutable reference to an element instead of using
/// the indexing notation.
/// Also, mutable slicing is not supported and updates must be done
/// element-wise.
pub struct ObservableVecDeque<T, Codec = crate::codec::Default> {
    v: VecDeque<T>,
    tx: rch::broadcast::Sender<VecDequeEvent<T>, Codec>,
    change: ChangeSender,
    on_err: Arc<dyn Fn(SendError) + Send + Sync>,
    done: bool,
}

impl<T, Codec> fmt::Debug for ObservableVecDeque<T, Codec>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.v.fmt(f)
    }
}

impl<T, Codec> From<VecDeque<T>> for ObservableVecDeque<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn from(v: VecDeque<T>) -> Self {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        Self { v, tx, change: ChangeSender::new(), on_err: Arc::new(default_on_err), done: false }
    }
}

impl<T, Codec> From<ObservableVecDeque<T, Codec>> for VecDeque<T> {
    fn from(ovd: ObservableVecDeque<T, Codec>) -> Self {
        ovd.v
    }
}

impl<T, Codec> Default for ObservableVecDeque<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    fn default() -> Self {
        Self::from(VecDeque::new())
    }
}

impl<T, Codec> ObservableVecDeque<T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    /// Creates an empty observable VecDeque.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty observable VecDeque with space for at least `capacity` elements.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::from(VecDeque::with_capacity(capacity))
    }

    /// Sets the error handler function that is called when sending an
    /// event fails.
    pub fn set_error_handler<E>(&mut self, on_err: E)
    where
        E: Fn(SendError) + Send + Sync + 'static,
    {
        self.on_err = Arc::new(on_err);
    }

    /// Subscribes to change events from this observable VecDeque.
    ///
    /// The current contents of the VecDeque is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe(&self, buffer: usize) -> VecDequeSubscription<T, Codec> {
        VecDequeSubscription::new(
            VecDequeInitialValue::new_value(self.v.clone()),
            if self.done { None } else { Some(self.tx.subscribe(buffer)) },
        )
    }

    /// Subscribes to change events from this observable VecDeque with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the VecDeque are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub fn subscribe_incremental(&self, buffer: usize) -> VecDequeSubscription<T, Codec> {
        VecDequeSubscription::new(
            VecDequeInitialValue::new_incremental(self.v.clone(), self.on_err.clone()),
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

    /// Appends an element at the back.
    ///
    /// A [VecDequeEvent::PushBack] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn push_back(&mut self, value: T) {
        self.assert_not_done();
        self.change.notify();

        send_event(&self.tx, &*self.on_err, VecDequeEvent::PushBack(value.clone()));
        self.v.push_back(value);
    }

    /// Appends an element at the front.
    ///
    /// A [VecDequeEvent::PushFront] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn push_front(&mut self, value: T) {
        self.assert_not_done();
        self.change.notify();

        send_event(&self.tx, &*self.on_err, VecDequeEvent::PushFront(value.clone()));
        self.v.push_front(value);
    }

    /// Removes the last element and returns it, or `None` if the VecDeque is empty.
    ///
    /// A [VecDequeEvent::PopBack] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn pop_back(&mut self) -> Option<T> {
        self.assert_not_done();

        match self.v.pop_back() {
            Some(value) => {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, VecDequeEvent::PopBack);
                Some(value)
            }
            None => None,
        }
    }

    /// Removes the first element and returns it, or `None` if the VecDeque is empty.
    ///
    /// A [VecDequeEvent::PopFront] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn pop_front(&mut self) -> Option<T> {
        self.assert_not_done();

        match self.v.pop_front() {
            Some(value) => {
                self.change.notify();
                send_event(&self.tx, &*self.on_err, VecDequeEvent::PopFront);
                Some(value)
            }
            None => None,
        }
    }

    /// Gets a mutable reference to the value at the specified index.
    ///
    /// A [VecDequeEvent::Set] change event is sent if the reference is accessed mutably.
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
    /// A [VecDequeEvent::Set] change event is sent for each value that is accessed mutably.
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
    /// A [VecDequeEvent::Insert] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or `index > len`.
    pub fn insert(&mut self, index: usize, value: T) {
        self.assert_not_done();

        let value_event = value.clone();
        self.v.insert(index, value);
        self.change.notify();
        send_event(&self.tx, &*self.on_err, VecDequeEvent::Insert(index, value_event));
    }

    /// Removes the element at the specified position, shifting all elements after it to the left.
    ///
    /// A [VecDequeEvent::Remove] change event is sent.
    ///
    /// Returns the removed value.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or the index is out of bounds.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        self.assert_not_done();

        let value = self.v.remove(index);
        if value.is_some() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecDequeEvent::Remove(index));
        }
        value
    }

    /// Removes the element at the specified position, replacing it with the last element.
    ///
    /// A [VecDequeEvent::SwapRemoveBack] change event is sent.
    ///
    /// Returns the removed value.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or the index is out of bounds.
    pub fn swap_remove_back(&mut self, index: usize) -> Option<T> {
        self.assert_not_done();

        let value = self.v.swap_remove_back(index);
        if value.is_some() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecDequeEvent::SwapRemoveBack(index));
        }
        value
    }

    /// Removes the element at the specified position, replacing it with the first element.
    ///
    /// A [VecDequeEvent::SwapRemoveFront] change event is sent.
    ///
    /// Returns the removed value.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before or the index is out of bounds.
    pub fn swap_remove_front(&mut self, index: usize) -> Option<T> {
        self.assert_not_done();

        let value = self.v.swap_remove_front(index);
        if value.is_some() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecDequeEvent::SwapRemoveFront(index));
        }
        value
    }

    /// Resizes the VecDeque to the specified length, filling each additional slot with the
    /// specified value.
    ///
    /// A [VecDequeEvent::Resize] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn resize(&mut self, new_len: usize, value: T) {
        self.assert_not_done();

        if new_len != self.v.len() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecDequeEvent::Resize(new_len, value.clone()));
            self.v.resize(new_len, value);
        }
    }

    /// Truncates the VecDeque to the specified length.
    ///
    /// If `new_len` is greater than the current length, this has no effect.
    ///
    /// A [VecDequeEvent::Truncate] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn truncate(&mut self, new_len: usize) {
        self.assert_not_done();

        if new_len < self.len() {
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecDequeEvent::Truncate(new_len));
            self.v.truncate(new_len);
        }
    }

    /// Removes all items.
    ///
    /// A [VecDequeEvent::Clear] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn clear(&mut self) {
        self.assert_not_done();

        if !self.v.is_empty() {
            self.v.clear();
            self.change.notify();
            send_event(&self.tx, &*self.on_err, VecDequeEvent::Clear);
        }
    }

    /// Retains only the elements specified by the predicate.
    ///
    /// A [VecDequeEvent::Retain] or [VecDequeEvent::RetainNot] change event is sent.
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
                send_event(&self.tx, &*self.on_err, VecDequeEvent::Retain(keep));
            } else {
                send_event(&self.tx, &*self.on_err, VecDequeEvent::RetainNot(remove));
            }
        }
    }

    /// Shrinks the capacity of the VecDeque as much as possible.
    ///
    /// A [VecDequeEvent::ShrinkToFit] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn shrink_to_fit(&mut self) {
        self.assert_not_done();
        send_event(&self.tx, &*self.on_err, VecDequeEvent::ShrinkToFit);
        self.v.shrink_to_fit()
    }

    /// Panics when `done` has been called.
    fn assert_not_done(&self) {
        if self.done {
            panic!("observable VecDeque cannot be changed after done has been called");
        }
    }

    /// Prevents further changes of this VecDeque and notifies
    /// are subscribers that no further events will occur.
    ///
    /// Methods that modify the VecDeque will panic after this has been called.
    /// It is still possible to subscribe to this observable VecDeque.
    pub fn done(&mut self) {
        if !self.done {
            send_event(&self.tx, &*self.on_err, VecDequeEvent::Done);
            self.done = true;
        }
    }

    /// Returns `true` if [done](Self::done) has been called and further
    /// changes are prohibited.
    ///
    /// Methods that modify the VecDeque will panic in this case.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Extracts the underlying VecDeque.
    ///
    /// If [done](Self::done) has not been called before this method,
    /// subscribers will receive an error.
    pub fn into_inner(self) -> VecDeque<T> {
        self.into()
    }
}

impl<T, Codec> Deref for ObservableVecDeque<T, Codec> {
    type Target = VecDeque<T>;

    fn deref(&self) -> &Self::Target {
        &self.v
    }
}

impl<T, Codec> Extend<T> for ObservableVecDeque<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.push_back(value);
        }
    }
}

/// A mutable reference to a value inside an [observable VecDeque](ObservableVecDeque).
///
/// A [VecDequeEvent::Set] change event is sent when this reference is dropped and the
/// value has been accessed mutably.
pub struct RefMut<'a, T, Codec>
where
    T: Clone + RemoteSend,
    Codec: crate::codec::Codec,
{
    index: usize,
    value: &'a mut T,
    changed: bool,
    tx: &'a rch::broadcast::Sender<VecDequeEvent<T>, Codec>,
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
            send_event(self.tx, self.on_err, VecDequeEvent::Set(self.index, self.value.clone()));
        }
    }
}

/// A mutable iterator over the items in an [observable VecDeque](ObservableVecDeque).
///
/// A [VecDequeEvent::Set] change event is sent for each value that is accessed mutably.
pub struct IterMut<'a, T, Codec> {
    inner: Enumerate<std::collections::vec_deque::IterMut<'a, T>>,
    tx: &'a rch::broadcast::Sender<VecDequeEvent<T>, Codec>,
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

struct MirroredVecDequeInner<T> {
    v: VecDeque<T>,
    complete: bool,
    done: bool,
    error: Option<RecvError>,
    max_size: usize,
}

impl<T> MirroredVecDequeInner<T>
where
    T: Clone,
{
    fn handle_event(&mut self, event: VecDequeEvent<T>) -> Result<(), RecvError> {
        match event {
            VecDequeEvent::InitialComplete => {
                self.complete = true;
            }
            VecDequeEvent::PushBack(v) => {
                self.v.push_back(v);
                if self.v.len() > self.max_size {
                    return Err(RecvError::MaxSizeExceeded(self.max_size));
                }
            }
            VecDequeEvent::PushFront(v) => {
                self.v.push_front(v);
                if self.v.len() > self.max_size {
                    return Err(RecvError::MaxSizeExceeded(self.max_size));
                }
            }
            VecDequeEvent::PopBack => {
                self.v.pop_back();
            }
            VecDequeEvent::PopFront => {
                self.v.pop_front();
            }
            VecDequeEvent::Insert(i, v) => {
                if i > self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.insert(i, v);
            }
            VecDequeEvent::Set(i, v) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v[i] = v;
            }
            VecDequeEvent::Remove(i) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.remove(i);
            }
            VecDequeEvent::SwapRemoveBack(i) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.swap_remove_back(i);
            }
            VecDequeEvent::SwapRemoveFront(i) => {
                if i >= self.v.len() {
                    return Err(RecvError::InvalidIndex(i));
                }
                self.v.swap_remove_front(i);
            }
            VecDequeEvent::Resize(l, v) => {
                self.v.resize(l, v);
            }
            VecDequeEvent::Truncate(l) => {
                self.v.truncate(l);
            }
            VecDequeEvent::Retain(r) => {
                let mut pos = 0;
                self.v.retain(|_| {
                    let keep_this = r.contains(&pos);
                    pos += 1;
                    keep_this
                });
            }
            VecDequeEvent::RetainNot(nr) => {
                let mut pos = 0;
                self.v.retain(|_| {
                    let keep_this = !nr.contains(&pos);
                    pos += 1;
                    keep_this
                });
            }
            VecDequeEvent::Clear => {
                self.v.clear();
            }
            VecDequeEvent::ShrinkToFit => {
                self.v.shrink_to_fit();
            }
            VecDequeEvent::Done => {
                self.done = true;
            }
        }
        Ok(())
    }
}

/// Initial value of an observable VecDeque subscription.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
enum VecDequeInitialValue<T, Codec = crate::codec::Default> {
    /// Initial value is present.
    Value(VecDeque<T>),
    /// Initial value is received incrementally.
    Incremental {
        /// Number of elements.
        len: usize,
        /// Receiver.
        rx: rch::mpsc::Receiver<T, Codec>,
    },
}

impl<T, Codec> VecDequeInitialValue<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Transmits the initial value as a whole.
    fn new_value(v: VecDeque<T>) -> Self {
        Self::Value(v)
    }

    /// Transmits the initial value incrementally.
    fn new_incremental(v: VecDeque<T>, on_err: Arc<dyn Fn(SendError) + Send + Sync>) -> Self {
        let (tx, rx) = rch::mpsc::channel(128);
        let len = v.len();

        exec::spawn(
            async move {
                for item in v.into_iter() {
                    match tx.send(item).await {
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

/// Observable VecDeque subscription.
///
/// This can be sent to a remote endpoint via a [remote channel](crate::rch).
/// Then, on the remote endpoint, [mirror](Self::mirror) can be used to build
/// and keep up-to-date a mirror of the observed VecDeque.
///
/// The event stream can also be processed event-wise using [recv](Self::recv).
/// If the subscription is not incremental [take_initial](Self::take_initial) must
/// be called before the first call to [recv](Self::recv).
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: crate::codec::Codec"))]
pub struct VecDequeSubscription<T, Codec = crate::codec::Default> {
    /// Value of VecDeque at time of subscription.
    initial: VecDequeInitialValue<T, Codec>,
    /// Initial value received completely.
    #[serde(skip, default)]
    complete: bool,
    /// Change events receiver.
    ///
    /// `None` if [ObservableVecDeque::done] has been called before subscribing.
    events: Option<rch::broadcast::Receiver<VecDequeEvent<T>, Codec>>,
    /// Event stream ended.
    #[serde(skip, default)]
    done: bool,
}

impl<T, Codec> VecDequeSubscription<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn new(
        initial: VecDequeInitialValue<T, Codec>,
        events: Option<rch::broadcast::Receiver<VecDequeEvent<T>, Codec>>,
    ) -> Self {
        Self { initial, complete: false, events, done: false }
    }

    /// Returns whether the subscription is incremental.
    pub fn is_incremental(&self) -> bool {
        matches!(self.initial, VecDequeInitialValue::Incremental { .. })
    }

    /// Returns whether the initial value event or
    /// stream of events that build up the initial value
    /// has completed or [take_initial](Self::take_initial) has been called.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Returns whether the observed VecDeque has indicated that no further
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
    pub fn take_initial(&mut self) -> Option<VecDeque<T>> {
        match &mut self.initial {
            VecDequeInitialValue::Value(value) if !self.complete => {
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
    pub async fn recv(&mut self) -> Result<Option<VecDequeEvent<T>>, RecvError> {
        // Provide initial value events.
        if !self.complete {
            match &mut self.initial {
                VecDequeInitialValue::Incremental { len, rx } => {
                    if *len > 0 {
                        match rx.recv().await? {
                            Some(v) => {
                                // Provide incremental initial value event.
                                *len -= 1;
                                return Ok(Some(VecDequeEvent::PushBack(v)));
                            }
                            None => return Err(RecvError::Closed),
                        }
                    } else {
                        // Provide incremental initial value complete event.
                        self.complete = true;
                        return Ok(Some(VecDequeEvent::InitialComplete));
                    }
                }
                VecDequeInitialValue::Value(_) => {
                    panic!("take_initial must be called before recv for non-incremental subscription");
                }
            }
        }

        // Provide change event.
        if let Some(rx) = &mut self.events {
            match rx.recv().await? {
                VecDequeEvent::Done => self.events = None,
                evt => return Ok(Some(evt)),
            }
        }

        // Provide done event.
        if self.done {
            Ok(None)
        } else {
            self.done = true;
            Ok(Some(VecDequeEvent::Done))
        }
    }
}

impl<T, Codec> VecDequeSubscription<T, Codec>
where
    T: RemoteSend + Clone + Sync,
    Codec: crate::codec::Codec,
{
    /// Mirror the VecDeque that this subscription is observing.
    ///
    /// `max_size` specifies the maximum allowed size of the mirrored collection.
    /// If this size is reached, processing of events is stopped and
    /// [RecvError::MaxSizeExceeded] is returned.
    pub fn mirror(mut self, max_size: usize) -> MirroredVecDeque<T, Codec> {
        let (tx, _rx) = rch::broadcast::channel::<_, _, { rch::DEFAULT_BUFFER }>(1);
        let (changed_tx, changed_rx) = watch::channel(());
        let (dropped_tx, mut dropped_rx) = oneshot::channel();

        // Build initial state.
        let inner = Arc::new(RwLock::new(Some(MirroredVecDequeInner {
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

        MirroredVecDeque { inner, tx: tx.downgrade(), changed_rx, _dropped_tx: dropped_tx }
    }
}

/// A VecDeque that is mirroring an observable VecDeque.
pub struct MirroredVecDeque<T, Codec = crate::codec::Default> {
    inner: Arc<RwLock<Option<MirroredVecDequeInner<T>>>>,
    tx: rch::broadcast::WeakSender<VecDequeEvent<T>, Codec>,
    changed_rx: watch::Receiver<()>,
    _dropped_tx: oneshot::Sender<()>,
}

impl<T, Codec> fmt::Debug for MirroredVecDeque<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MirroredVecDeque").finish()
    }
}

impl<T, Codec> MirroredVecDeque<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Returns a reference to the current value of the VecDeque.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed VecDeque has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableVecDeque::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow(&self) -> Result<MirroredVecDequeRef<'_, T>, RecvError> {
        let inner = self.inner.read().await;
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredVecDequeRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Returns a reference to the current value of the VecDeque and marks it as seen.
    ///
    /// Thus [changed](Self::changed) will not return immediately until the value changes
    /// after this method returns.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed VecDeque has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableVecDeque::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow_and_update(&mut self) -> Result<MirroredVecDequeRef<'_, T>, RecvError> {
        let inner = self.inner.read().await;
        self.changed_rx.borrow_and_update();
        let inner = RwLockReadGuard::map(inner, |inner| inner.as_ref().unwrap());
        match &inner.error {
            None => Ok(MirroredVecDequeRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Stops updating the VecDeque and returns its current contents.
    pub async fn detach(self) -> VecDeque<T> {
        let mut inner = self.inner.write().await;
        inner.take().unwrap().v
    }

    /// Waits for a change and marks the newest value as seen.
    ///
    /// This also returns when connection to the observed VecDeque has been lost
    /// or the VecDeque has been marked as done.
    pub async fn changed(&mut self) {
        let _ = self.changed_rx.changed().await;
    }

    /// Subscribes to change events from this mirrored VecDeque.
    ///
    /// The current contents of the VecDeque is included with the subscription.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe(&self, buffer: usize) -> Result<VecDequeSubscription<T, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(VecDequeSubscription::new(VecDequeInitialValue::new_value(initial), events))
    }

    /// Subscribes to change events from this mirrored VecDeque with incremental sending
    /// of the current contents.
    ///
    /// The current contents of the VecDeque are sent incrementally.
    ///
    /// `buffer` specifies the maximum size of the event buffer for this subscription in number of events.
    /// If it is exceeded the subscription is shed and the receiver gets a [RecvError::Lagged].
    pub async fn subscribe_incremental(
        &self, buffer: usize,
    ) -> Result<VecDequeSubscription<T, Codec>, RecvError> {
        let view = self.borrow().await?;
        let initial = view.clone();
        let events = if view.is_done() { None } else { self.tx.upgrade().map(|tx| tx.subscribe(buffer)) };

        Ok(VecDequeSubscription::new(
            VecDequeInitialValue::new_incremental(initial, Arc::new(default_on_err)),
            events,
        ))
    }
}

impl<T, Codec> Drop for MirroredVecDeque<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

/// A snapshot view of an observable VecDeque.
pub struct MirroredVecDequeRef<'a, T>(RwLockReadGuard<'a, MirroredVecDequeInner<T>>);

impl<T> MirroredVecDequeRef<'_, T> {
    /// Returns `true` if the initial state of an incremental subscription has
    /// been reached.
    pub fn is_complete(&self) -> bool {
        self.0.complete
    }

    /// Returns `true` if the observed VecDeque has been marked as done by calling
    /// [ObservableVecDeque::done] and thus no further changes can occur.
    pub fn is_done(&self) -> bool {
        self.0.done
    }
}

impl<T> fmt::Debug for MirroredVecDequeRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.v.fmt(f)
    }
}

impl<T> Deref for MirroredVecDequeRef<'_, T> {
    type Target = VecDeque<T>;

    fn deref(&self) -> &Self::Target {
        &self.0.v
    }
}

impl<T> Drop for MirroredVecDequeRef<'_, T> {
    fn drop(&mut self) {
        // required for drop order
    }
}
