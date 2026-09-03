use std::fmt;
use tracing::Instrument;
use wokio::task::JoinHandle;

use super::{
    ReadLock, RwLock,
    msg::{ReadRequest, Value, WriteRequest},
};
use crate::{
    RemoteSend, codec,
    rch::{mpsc, watch},
};

/// The owner of [RwLock]s holding a shared value.
///
/// All acquired locks become invalid when this is dropped.
///
/// See [module-level documentation](super) for details.
pub struct Owner<T, Codec = codec::Default> {
    task: Option<JoinHandle<T>>,
    rw_lock: RwLock<T, Codec>,
    term_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl<T, Codec> fmt::Debug for Owner<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owner").finish()
    }
}

impl<T, Codec> Owner<T, Codec>
where
    T: RemoteSend + Clone + Sync,
    Codec: codec::Codec,
{
    /// Creates a new [RwLock] owner with the specified shared value.
    pub fn new(mut value: T) -> Self {
        let (read_req_tx, read_req_rx) = mpsc::with_local_buffer(1);
        let read_req_tx = read_req_tx.set_buffer();
        let read_req_rx = read_req_rx.set_buffer();
        let (write_req_tx, write_req_rx) = mpsc::with_local_buffer(1);
        let write_req_tx = write_req_tx.set_buffer();
        let write_req_rx = write_req_rx.set_buffer();
        let (term_tx, term_rx) = tokio::sync::oneshot::channel();

        let task = wokio::spawn(
            async move {
                tokio::select! {
                    biased;
                    _ = term_rx => (),
                    _ = Self::owner_task(&mut value, read_req_rx, write_req_rx) => (),
                }
                value
            }
            .instrument(crate::util::task_span!(::tracing::Level::TRACE, "rw_lock_owner")),
        );

        let read_lock = ReadLock::new(read_req_tx);
        let rw_lock = RwLock::new(read_lock, write_req_tx);

        Self { task: Some(task), rw_lock, term_tx: Some(term_tx) }
    }

    /// Message handler for lock owner.
    async fn owner_task(
        value: &mut T, mut read_req_rx: mpsc::Receiver<ReadRequest<T, Codec>, Codec, 1>,
        mut write_req_rx: mpsc::Receiver<WriteRequest<T, Codec>, Codec, 1>,
    ) {
        let (dropped_tx, dropped_rx) = mpsc::with_local_buffer(1);
        let mut dropped_tx = dropped_tx.set_buffer();
        let mut dropped_rx = dropped_rx.set_buffer::<1>();
        let (mut invalid_tx, mut invalid_rx) = watch::channel(false);

        loop {
            tokio::select! {
                biased;

                // Write value request.
                res = write_req_rx.recv() => {
                    let WriteRequest {value_tx, new_value_rx, confirm_tx} = match res {
                        Ok(Some(req)) => req,
                        Ok(None) => break,
                        Err(err) if err.is_disconnected() => break,
                        Err(_) => continue,
                    };

                    // Invalidate current value.
                    let _ = invalid_tx.send(true);

                    // Wait for drop confirmation from all lock holders.
                    drop(dropped_tx);
                    loop {
                        if let Ok(None) = dropped_rx.recv().await {
                            break;
                        }
                    }

                    // Create new dropped notification channel.
                    let (new_dropped_tx, new_dropped_rx) = mpsc::with_local_buffer(1);
                    let new_dropped_tx = new_dropped_tx.set_buffer();
                    let new_dropped_rx = new_dropped_rx.set_buffer();
                    dropped_tx = new_dropped_tx;
                    dropped_rx = new_dropped_rx;

                    // Create new invalidation channel.
                    let (new_invalid_tx, new_invalid_rx) = watch::channel(false);
                    invalid_tx = new_invalid_tx;
                    invalid_rx = new_invalid_rx;

                    // Send current value for writing.
                    let _ = value_tx.send(value.clone());

                    // Wait for modified value and store it.
                    if let Ok(nv) = new_value_rx.await {
                        *value = nv;

                        // Send confirmation.
                        let _ = confirm_tx.send(());
                    }
                },

                // Read value request.
                res = read_req_rx.recv() => {
                    let ReadRequest {value_tx} = match res {
                        Ok(Some(req)) => req,
                        Ok(None) => break,
                        Err(err) if err.is_disconnected() => break,
                        Err(_) => continue,
                    };

                    // Send current value together with invalidation channels.
                    let v = Value {
                        value: value.clone(),
                        dropped_tx: dropped_tx.clone(),
                        invalid_rx: invalid_rx.clone(),
                    };
                    let _ = value_tx.send(v);
                },
            }
        }
    }

    /// Makes all acquired locks invalid and returns the shared value.
    ///
    /// Existing lock handles can no longer acquire the value after this method
    /// completes. Any uncommitted changes held by a write guard are not applied.
    pub async fn into_inner(mut self) -> T {
        let _ = self.term_tx.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap()
    }

    /// Returns a new read/write lock for the shared value.
    pub fn rw_lock(&self) -> RwLock<T, Codec> {
        self.rw_lock.clone()
    }

    /// Returns a new read lock for the shared value.
    pub fn read_lock(&self) -> ReadLock<T, Codec> {
        self.rw_lock.read_lock()
    }
}

impl<T, Codec> Drop for Owner<T, Codec> {
    fn drop(&mut self) {
        // empty
    }
}
