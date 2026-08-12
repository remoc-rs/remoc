//! Messages exchanged between read/write locks and the owner.

use crate::{
    RemoteSend, codec,
    rch::{mpsc, oneshot, watch},
};

/// A read request from a lock to the owner.
#[derive(Debug)]
pub(super) struct ReadRequest<T, Codec> {
    /// Channel for sending the value.
    pub(crate) value_tx: oneshot::Sender<Value<T, Codec>, Codec>,
}

crate::versioned::compact::impl_struct! {
    ReadRequest<T, Codec>,
    fields {
        value_tx: oneshot::Sender<Value<T, Codec>, Codec> => "_0",
    }
    where T: RemoteSend, Codec: codec::Codec
}

/// A write request from a lock to the owner.
#[derive(Debug)]
pub(super) struct WriteRequest<T, Codec> {
    /// Channel for sending current value.
    pub(super) value_tx: oneshot::Sender<T, Codec>,
    /// Channel for receiving modified value.
    pub(super) new_value_rx: oneshot::Receiver<T, Codec>,
    /// Channel for confirming that modified value has been stored.
    pub(super) confirm_tx: oneshot::Sender<(), Codec>,
}

crate::versioned::compact::impl_struct! {
    WriteRequest<T, Codec>,
    fields {
        value_tx: oneshot::Sender<T, Codec> => "_0",
        new_value_rx: oneshot::Receiver<T, Codec> => "_1",
        confirm_tx: oneshot::Sender<(), Codec> => "_2",
    }
    where T: RemoteSend, Codec: codec::Codec
}

/// A value together with invalidation channels.
#[derive(Clone)]
pub(super) struct Value<T, Codec> {
    /// The shared value.
    pub(super) value: T,
    /// Notification channel that all instances of this value have been dropped.
    pub(super) dropped_tx: mpsc::Sender<(), Codec, 1>,
    /// Notification channel that value has been invalidated by the owner.
    pub(super) invalid_rx: watch::Receiver<bool, Codec>,
}

crate::versioned::compact::impl_struct! {
    Value<T, Codec>,
    fields {
        value: T => "_0",
        dropped_tx: mpsc::Sender<(), Codec, 1> => "_1",
        invalid_rx: watch::Receiver<bool, Codec> => "_2",
    }
    where T: RemoteSend, Codec: codec::Codec
}

impl<T, Codec> Value<T, Codec>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// True, if value is valid.
    pub(crate) fn is_valid(&self) -> bool {
        if self.dropped_tx.is_closed() {
            return false;
        }

        match self.invalid_rx.borrow() {
            Ok(invalid) if !*invalid => (),
            _ => return false,
        }

        true
    }
}
