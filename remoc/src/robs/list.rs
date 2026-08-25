//! Observable append-only list.
//!
//! This provides a locally and remotely observable append-only list.
//! The observable list sends a change event each time a change is performed on it.
//! The [resulting event stream](ListSubscription) can either be processed event-wise
//! or used to build a [mirrored list](MirroredList).
//!
//! # Alternatives
//!
//! The [observable vector](super::vec) provides most operations of a [vector](Vec),
//! but allocates a separate event buffer per subscriber and thus uses more memory.
//!
//! # Basic use
//!
//! Create a [ObservableList] and obtain a [subscription](ListSubscription) to it using
//! [ObservableList::subscribe].
//! Send this subscription to a remote endpoint via a [remote channel](crate::rch) and call
//! [ListSubscription::mirror] on the remote endpoint to obtain a live mirror of the observed
//! vector or process each change event individually using [ListSubscription::recv].
//!
//! # Example
//!
//! In the following example the client mirrors an append-only list held by the server.
//!
//! ```
//! use remoc::prelude::*;
//! use remoc::robs::list::{ListSubscription, ObservableList};
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<ListSubscription<String>>) {
//!     let mut list: ObservableList<String> = ObservableList::new();
//!     list.push("first".to_string());
//!
//!     // The subscription conveys the current contents of the list,
//!     // followed by every subsequently appended item.
//!     tx.send(list.subscribe()).await.unwrap();
//!
//!     list.push("second".to_string());
//!
//!     // Tells subscribers that no further items will follow.
//!     list.done();
//! }
//!
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<ListSubscription<String>>) {
//!     let sub = rx.recv().await.unwrap().unwrap();
//!     let mut mirror = sub.mirror(1000);
//!
//!     // Wait until the server has appended all items.
//!     mirror.done().await.unwrap();
//!
//!     let list = mirror.borrow().await.unwrap();
//!     assert_eq!(*list, vec!["first".to_string(), "second".to_string()]);
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```
//!

use futures::{Future, FutureExt, future};
use std::{
    fmt,
    marker::PhantomData,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, RwLockReadGuard, mpsc, oneshot, watch};
use tracing::Instrument;

use super::{ChangeNotifier, ChangeSender, RecvError, SendError, default_on_err};
use crate::prelude::*;

/// A list change event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListEvent<T> {
    /// An item was pushed at the end of the list.
    Push(T),
    /// The list has reached its final state and
    /// no further events will occur.
    Done,
    /// The subscription has reached the value of the observed
    /// list at the time it was subscribed.
    InitialComplete,
}

crate::versioned::compact::impl_enum! {
    ListEvent<T>,
    variants {
        Push(value: T) => "_0",
        Done => "_1",
        #[skip]
        InitialComplete,
    }
    where T: RemoteSend
}

/// Observable list task request.
enum Req<T> {
    /// Push item.
    Push(T),
    /// List is complete, i.e. no more items will be sent.
    Done,
    /// Borrow the current list.
    Borrow(oneshot::Sender<OwnedMutexGuard<Vec<T>>>),
    /// Set the error handler.
    SetErrorHandler(Box<dyn Fn(SendError) + Send + Sync + 'static>),
}

/// Oberservable list subscribing related task request.
enum DistReq<T, Codec> {
    /// Subscribe receiver.
    Subscribe(rch::mpsc::Sender<ListEvent<T>, Codec>),
    /// Notify when no subscribers are left.
    NotifyNoSubscribers(oneshot::Sender<()>),
}

/// An observable list distributor allows subscribing to an observable list.
///
/// This is clonable and can be sent to other tasks.
#[derive(Clone)]
pub struct ObservableListDistributor<T, Codec = crate::codec::Default> {
    tx: mpsc::UnboundedSender<DistReq<T, Codec>>,
    len: Arc<AtomicUsize>,
    subscriber_count: Arc<AtomicUsize>,
}

impl<T, Codec> fmt::Debug for ObservableListDistributor<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ObservableListDistributor")
            .field("len", &self.len.load(Ordering::Relaxed))
            .field("subscriber_count", &self.subscriber_count.load(Ordering::Relaxed))
            .finish()
    }
}

impl<T, Codec> ObservableListDistributor<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Sends a request to the task.
    fn req(&self, req: DistReq<T, Codec>) {
        if self.tx.send(req).is_err() {
            tracing::warn!("observable list task was terminated");
        }
    }

    /// Subscribes to the observable list with incremental sending of the current contents.
    pub fn subscribe(&self) -> ListSubscription<T, Codec> {
        let (tx, rx) = rch::mpsc::channel();
        let _ = self.tx.send(DistReq::Subscribe(tx));
        ListSubscription::new(self.len.load(Ordering::Relaxed), rx)
    }

    /// Current number of subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Returns when all subscribers have quit.
    ///
    /// If no subscribers are currently present, this return immediately.
    /// This also returns when [done](ObservableList::done) has been called and
    /// all subscribers have received all elements of the list.
    pub fn closed(&self) -> impl Future<Output = ()> + use<T, Codec> {
        let (tx, rx) = oneshot::channel();
        self.req(DistReq::NotifyNoSubscribers(tx));
        async move {
            let _ = rx.await;
        }
    }

    /// Returns `true` if there are currently no subscribers.
    pub fn is_closed(&self) -> bool {
        self.subscriber_count() == 0
    }
}

/// An append-only list that emits an event for each change.
///
/// Use [subscribe](Self::subscribe) to obtain an event stream
/// that can be used for building a mirror of this list.
///
/// The [distributor method](Self::distributor) can be used to obtain a clonable object
/// that can be used to make subscriptions from other tasks.
pub struct ObservableList<T, Codec = crate::codec::Default> {
    tx: mpsc::UnboundedSender<Req<T>>,
    change: ChangeSender,
    len: Arc<AtomicUsize>,
    done: bool,
    dist: ObservableListDistributor<T, Codec>,
}

impl<T, Codec> fmt::Debug for ObservableList<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ObservableList")
            .field("len", &self.len.load(Ordering::Relaxed))
            .field("done", &self.done)
            .field("subscriber_count", &self.dist.subscriber_count.load(Ordering::Relaxed))
            .finish()
    }
}

impl<T, Codec> Default for ObservableList<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl<T, Codec> From<Vec<T>> for ObservableList<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn from(initial: Vec<T>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (sub_tx, sub_rx) = mpsc::unbounded_channel();
        let len = Arc::new(AtomicUsize::new(initial.len()));
        let subscriber_count = Arc::new(AtomicUsize::new(0));
        wokio::spawn(Self::task(initial, rx, sub_rx, subscriber_count.clone()).in_current_span());
        Self {
            tx,
            change: ChangeSender::new(),
            len: len.clone(),
            done: false,
            dist: ObservableListDistributor { tx: sub_tx, len, subscriber_count },
        }
    }
}

impl<T, Codec> ObservableList<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    /// Creates a new, empty observable list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends a request to the task.
    fn req(&self, req: Req<T>) {
        if self.tx.send(req).is_err() {
            tracing::warn!("observable list task was terminated");
        }
    }

    /// Panics when `done` has been called.
    fn assert_not_done(&self) {
        if self.done {
            panic!("observable list cannot be changed after done has been called");
        }
    }

    /// Sets the error handler function that is called when sending an
    /// event fails.
    pub fn set_error_handler<E>(&mut self, on_err: E)
    where
        E: Fn(SendError) + Send + Sync + 'static,
    {
        self.req(Req::SetErrorHandler(Box::new(on_err)));
    }

    /// Returns an observable list distributor that can be used to make subscriptions
    /// to this observable list.
    ///
    /// It is clonable and can be sent to other tasks.
    pub fn distributor(&self) -> ObservableListDistributor<T, Codec> {
        self.dist.clone()
    }

    /// Subscribes to the observable list with incremental sending of the current contents.
    pub fn subscribe(&self) -> ListSubscription<T, Codec> {
        self.dist.subscribe()
    }

    /// Current number of subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.dist.subscriber_count()
    }

    /// Returns a [change notifier](ChangeNotifier) that can be used *locally* to be
    /// notified of changes to this collection.
    pub fn notifier(&self) -> ChangeNotifier {
        self.change.subscribe()
    }

    /// Returns when all subscribers have quit.
    ///
    /// If no subscribers are currently present, this return immediately.
    /// This also returns when [done](Self::done) has been called and
    /// all subscribers have received all elements of the list.
    pub fn closed(&self) -> impl Future<Output = ()> + use<T, Codec> {
        self.dist.closed()
    }

    /// Returns `true` if there are currently no subscribers.
    pub fn is_closed(&self) -> bool {
        self.dist.is_closed()
    }

    /// Appends an element at the end.
    ///
    /// A [ListEvent::Push] change event is sent.
    ///
    /// # Panics
    /// Panics when [done](Self::done) has been called before.
    pub fn push(&mut self, value: T) {
        self.assert_not_done();
        self.req(Req::Push(value));
        self.len.fetch_add(1, Ordering::Relaxed);
        self.change.notify();
    }

    /// The current number of elements in the observable list.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Returns whether this observable list is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Prevents further changes of this list and notifies
    /// are subscribers that no further events will occur.
    ///
    /// Methods that modify the list will panic after this has been called.
    /// It is still possible to subscribe to this observable list.
    pub fn done(&mut self) {
        if !self.done {
            self.req(Req::Done);
            self.done = true;
        }
    }

    /// Returns `true` if [done](Self::done) has been called and further
    /// changes are prohibited.
    ///
    /// Methods that modify the list will panic in this case.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Borrows the current value of the observable list.
    ///
    /// While the borrow is held sending of events to subscribers is paused.
    #[allow(clippy::needless_lifetimes)]
    pub async fn borrow<'a>(&'a self) -> ObservableListRef<'a, T> {
        let (tx, rx) = oneshot::channel();
        self.req(Req::Borrow(tx));
        ObservableListRef { buffer: rx.await.unwrap(), _phantom: PhantomData }
    }

    /// Event dispatch task.
    async fn task(
        buffer: Vec<T>, rx: mpsc::UnboundedReceiver<Req<T>>, dist_rx: mpsc::UnboundedReceiver<DistReq<T, Codec>>,
        subscriber_count: Arc<AtomicUsize>,
    ) {
        /// State of a subscription.
        struct SubState<T, Codec> {
            pos: usize,
            done: bool,
            tx: rch::mpsc::Sender<ListEvent<T>, Codec>,
        }

        let buffer_shared = Arc::new(Mutex::new(buffer));
        let mut buffer_guard_opt = None;
        let mut rx_opt = Some(rx);
        let mut dist_rx_opt = Some(dist_rx);
        let mut subs: Vec<SubState<T, Codec>> = Vec::new();
        let mut done = false;
        let mut on_err: Box<dyn Fn(SendError) + Send + Sync + 'static> = Box::new(default_on_err);
        let mut no_sub_notify: Vec<oneshot::Sender<()>> = Vec::new();

        // Event and transmit loop.
        loop {
            // Obtain buffer mutex.
            let buffer = match &mut buffer_guard_opt {
                Some(br) => br,
                None => {
                    buffer_guard_opt = Some(buffer_shared.clone().lock_owned().await);
                    buffer_guard_opt.as_mut().unwrap()
                }
            };

            // If no more items can arrive, keep only subscription that have
            // not yet received all items.
            if rx_opt.is_none() {
                if done {
                    subs.retain(|sub| !sub.done);
                } else {
                    subs.retain(|sub| sub.pos < buffer.len());
                }
            }

            // Update subscriber counts.
            subscriber_count.store(subs.len(), Ordering::Relaxed);
            if subs.is_empty() {
                for tx in no_sub_notify.drain(..) {
                    let _ = tx.send(());
                }
            }

            // Create tasks that obtain send permits.
            let mut permit_tasks = Vec::new();
            for (i, sub) in subs.iter().enumerate() {
                if sub.pos < buffer.len() || (done && !sub.done) {
                    permit_tasks.push(async move { (i, sub.tx.reserve().await) }.boxed());
                }
            }

            // Check for termination.
            if rx_opt.is_none() && dist_rx_opt.is_none() && permit_tasks.is_empty() {
                break;
            }

            tokio::select! {
                biased;

                // Process request.
                req = async {
                    match &mut rx_opt {
                        Some(rx) => rx.recv().await,
                        None => future::pending().await,
                    }
                } => match req {
                    Some(Req::Push(v)) => buffer.push(v),
                    Some(Req::Done) => done = true,
                    Some(Req::Borrow(tx)) => {
                        let _ = tx.send(buffer_guard_opt.take().unwrap());
                    }
                    Some(Req::SetErrorHandler(handler)) => on_err = handler,
                    None => rx_opt = None,
                },

                // Process distribution request.
                req = async {
                    match &mut dist_rx_opt {
                        Some(rx) => rx.recv().await,
                        None => future::pending().await,
                    }
                } => match req {
                    Some(DistReq::Subscribe(tx)) => subs.push(SubState { pos: 0, done: false, tx }),
                    Some(DistReq::NotifyNoSubscribers(tx)) => no_sub_notify.push(tx),
                    None => dist_rx_opt = None,
                },

                // Process send permit ready.
                (i, res) = async move {
                    if permit_tasks.is_empty() {
                        future::pending().await
                    } else {
                        future::select_all(permit_tasks).await.0
                    }
                } => match res {
                    Ok(permit) => {
                        let sub = &mut subs[i];
                        if sub.pos < buffer.len() {
                            permit.send(ListEvent::Push(buffer[sub.pos].clone()));
                            sub.pos += 1;
                        } else if done && !sub.done {
                            permit.send(ListEvent::Done);
                            sub.done = true;
                        } else {
                            unreachable!()
                        }
                    }
                    Err(err) => {
                        subs.swap_remove(i);
                        if let Ok(err) = SendError::try_from(err) {
                            on_err(err);
                        }
                    }
                },
            }
        }
    }
}

impl<T, Codec> Drop for ObservableList<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}

impl<T, Codec> Extend<T> for ObservableList<T, Codec>
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

/// A reference to an observable list.
///
/// While this is held sending of events to subscribers is paused.
pub struct ObservableListRef<'a, T> {
    buffer: OwnedMutexGuard<Vec<T>>,
    _phantom: PhantomData<&'a ()>,
}

impl<T> fmt::Debug for ObservableListRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.buffer.fmt(f)
    }
}

impl<T> Deref for ObservableListRef<'_, T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

struct MirroredListInner<T> {
    v: Vec<T>,
    complete: bool,
    done: bool,
    error: Option<RecvError>,
    max_size: usize,
}

impl<T> MirroredListInner<T> {
    fn handle_event(&mut self, event: ListEvent<T>) -> Result<(), RecvError> {
        match event {
            ListEvent::InitialComplete => {
                self.complete = true;
            }
            ListEvent::Push(v) => {
                self.v.push(v);
                if self.v.len() > self.max_size {
                    return Err(RecvError::MaxSizeExceeded(self.max_size));
                }
            }
            ListEvent::Done => {
                self.done = true;
            }
        }
        Ok(())
    }
}

/// Observable list subscription.
///
/// This can be sent to a remote endpoint via a [remote channel](crate::rch).
/// Then, on the remote endpoint, [mirror](Self::mirror) can be used to build
/// and keep up-to-date a mirror of the observed list.
///
/// The event stream can also be processed event-wise using [recv](Self::recv).
#[derive(Debug)]
pub struct ListSubscription<T, Codec = crate::codec::Default> {
    /// Length of list at time of subscription.
    initial_len: usize,
    /// Initial value received completely.
    complete: bool,
    /// Change events receiver.
    events: Option<rch::mpsc::Receiver<ListEvent<T>, Codec>>,
    /// Number of elements received so far.
    len: usize,
}

crate::versioned::compact::impl_struct! {
    ListSubscription<T, Codec>,
    fields {
        initial_len: usize => "_0",
        events: Option<rch::mpsc::Receiver<ListEvent<T>, Codec>> => "_1",
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::if_default_ref")]
        len: usize => "_2",
    }
    default { complete }
    where T: RemoteSend, Codec: crate::codec::Codec
}

impl<T, Codec> ListSubscription<T, Codec>
where
    T: RemoteSend + Clone,
    Codec: crate::codec::Codec,
{
    fn new(initial_len: usize, events: rch::mpsc::Receiver<ListEvent<T>, Codec>) -> Self {
        Self { initial_len, complete: false, events: Some(events), len: 0 }
    }

    /// Returns whether the initial value event or
    /// stream of events that build up the initial value
    /// has completed.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Returns whether the observed list has indicated that no further
    /// change events will occur.
    pub fn is_done(&self) -> bool {
        self.events.is_none()
    }

    /// Receives the next change event.
    ///
    /// Returns [`None`] after the observed list has been marked as done and all
    /// preceding events have been received. Transfer failures are reported as
    /// [`RecvError`].
    pub async fn recv(&mut self) -> Result<Option<ListEvent<T>>, RecvError> {
        // Provide initial value complete event.
        if self.len == self.initial_len && !self.complete {
            self.complete = true;
            return Ok(Some(ListEvent::InitialComplete));
        }

        // Provide change event.
        match &mut self.events {
            Some(rx) => match rx.recv().await? {
                Some(ListEvent::InitialComplete) => unreachable!(),
                Some(evt @ ListEvent::Push(_)) => {
                    self.len += 1;
                    Ok(Some(evt))
                }
                Some(ListEvent::Done) => {
                    self.events = None;
                    Ok(Some(ListEvent::Done))
                }
                None => Err(RecvError::Closed),
            },
            None => Ok(None),
        }
    }

    /// Receives the next item.
    ///
    /// `Ok(None)` is returned when all items have been received and the observed
    /// list has been marked as done.
    pub async fn recv_item(&mut self) -> Result<Option<T>, RecvError> {
        loop {
            match self.recv().await? {
                Some(ListEvent::InitialComplete) => (),
                Some(ListEvent::Push(item)) => return Ok(Some(item)),
                Some(ListEvent::Done) => (),
                None => return Ok(None),
            }
        }
    }
}

impl<T, Codec> ListSubscription<T, Codec>
where
    T: RemoteSend + Clone + Sync,
    Codec: crate::codec::Codec,
{
    /// Mirror the list that this subscription is observing.
    ///
    /// `max_size` specifies the maximum allowed size of the mirrored collection.
    /// If this size is reached, processing of events is stopped and
    /// [RecvError::MaxSizeExceeded] is returned.
    pub fn mirror(mut self, max_size: usize) -> MirroredList<T> {
        let (changed_tx, changed_rx) = watch::channel(());
        let (dropped_tx, mut dropped_rx) = mpsc::channel(1);

        // Build initial state.
        let inner = Arc::new(RwLock::new(MirroredListInner {
            v: Vec::new(),
            complete: false,
            done: false,
            error: None,
            max_size,
        }));
        let inner_task = Arc::downgrade(&inner);

        // Process change events.
        wokio::spawn(
            async move {
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = dropped_rx.recv() => return,
                        event = self.recv() => event,
                    };

                    let inner = match inner_task.upgrade() {
                        Some(inner) => inner,
                        None => return,
                    };
                    let mut inner = inner.write().await;

                    changed_tx.send_replace(());

                    match event {
                        Ok(Some(event)) => {
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

        MirroredList { inner: Some(inner), changed_rx, _dropped_tx: dropped_tx }
    }
}

/// An append-only list that is mirroring an observable append-only list.
///
/// Clones of this are cheap and share the same underlying mirrored list.
#[derive(Clone)]
pub struct MirroredList<T> {
    inner: Option<Arc<RwLock<MirroredListInner<T>>>>,
    changed_rx: watch::Receiver<()>,
    _dropped_tx: mpsc::Sender<()>,
}

impl<T> fmt::Debug for MirroredList<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MirroredList").finish()
    }
}

impl<T> MirroredList<T>
where
    T: RemoteSend + Clone,
{
    /// Returns a reference to the current value of the list.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed list has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableList::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow(&self) -> Result<MirroredListRef<'_, T>, RecvError> {
        let inner = self.inner.as_ref().unwrap().read().await;
        match &inner.error {
            None => Ok(MirroredListRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Returns a reference to the current value of the list and marks it as seen.
    ///
    /// Thus [changed](Self::changed) will not return immediately until the value changes
    /// after this method returns.
    ///
    /// Updates are paused while the read lock is held.
    ///
    /// This method returns an error if the observed list has been dropped
    /// or the connection to it failed before it was marked as done by calling
    /// [ObservableList::done].
    /// In this case the mirrored contents at the point of loss of connection
    /// can be obtained using [detach](Self::detach).
    pub async fn borrow_and_update(&mut self) -> Result<MirroredListRef<'_, T>, RecvError> {
        let inner = self.inner.as_ref().unwrap().read().await;
        self.changed_rx.borrow_and_update();
        match &inner.error {
            None => Ok(MirroredListRef(inner)),
            Some(err) => Err(err.clone()),
        }
    }

    /// Stops updating the list and returns its current contents.
    ///
    /// If clones of this mirrored list exist, the cloned contents are returned.
    /// The result includes updates already applied to the mirror but this method
    /// does not wait for additional remote updates.
    pub async fn detach(mut self) -> Vec<T> {
        match Arc::try_unwrap(self.inner.take().unwrap()) {
            Ok(inner) => inner.into_inner().v,
            Err(inner) => inner.read().await.v.clone(),
        }
    }

    /// Waits for a change (append of one or more elements) and marks the newest value as seen.
    ///
    /// This also returns when connection to the observed list has been lost
    /// or the list has been marked as done.
    pub async fn changed(&mut self) {
        let _ = self.changed_rx.changed().await;
    }

    /// Waits for the observed list to be marked as done and for all elements to
    /// be received by this mirror of it.
    ///
    /// This marks changes as seen, even when aborted.
    pub async fn done(&mut self) -> Result<(), RecvError> {
        while !self.borrow_and_update().await?.is_done() {
            self.changed().await;
        }
        Ok(())
    }
}

impl<T> Drop for MirroredList<T> {
    fn drop(&mut self) {
        // empty
    }
}

/// A snapshot view of an observable append-only list.
pub struct MirroredListRef<'a, T>(RwLockReadGuard<'a, MirroredListInner<T>>);

impl<T> MirroredListRef<'_, T> {
    /// Returns `true` if the mirror list has reached the length of the observed
    /// list at the time of subscription.
    pub fn is_complete(&self) -> bool {
        self.0.complete
    }

    /// Returns `true` if the observed list has been marked as done by calling
    /// [ObservableList::done] and thus no further changes can occur.
    pub fn is_done(&self) -> bool {
        self.0.done
    }
}

impl<T> fmt::Debug for MirroredListRef<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.v.fmt(f)
    }
}

impl<T> Deref for MirroredListRef<'_, T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0.v
    }
}

impl<T> Drop for MirroredListRef<'_, T> {
    fn drop(&mut self) {
        // required for drop order
    }
}
