use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

use super::super::{ClosedReason, SendErrorExt, Sending, mpsc};
use crate::{RemoteSend, codec};

/// An error occurred during sending over an mpsc channel.
#[derive(Clone, custom_debug::Debug)]
pub enum SendError<T> {
    /// The remote end closed the channel.
    Closed(#[debug(skip)] T),
    /// Communication with the remote endpoint failed.
    Failed,
}

crate::versioned::impl_enum! {
    SendError<T>,
    versioner = crate::versioned::RemocCompact,
    variants {
        Closed(item: T) => "_0",
        Failed => "_1",
    }
    where T: RemoteSend
}

impl<T> SendError<T> {
    /// True, if the remote endpoint closed the channel.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }

    /// True, if the remote endpoint closed the channel, was dropped or the connection failed.
    ///
    /// This is always true since a oneshot channel has no method of reporting other errors
    /// (such as serialization errors) because the send operation is performed asynchronously.
    #[deprecated = "a remoc::rch::oneshot::SendError is always due to disconnection"]
    pub fn is_disconnected(&self) -> bool {
        true
    }

    /// Returns whether the error is final, i.e. no further send operation can succeed.
    #[deprecated = "a remoc::rch::oneshot::SendError is always final"]
    pub fn is_final(&self) -> bool {
        true
    }

    /// Returns the error without the contained item.
    pub fn without_item(self) -> SendError<()> {
        match self {
            Self::Closed(_) => SendError::Closed(()),
            Self::Failed => SendError::Failed,
        }
    }
}

impl<T> SendErrorExt for SendError<T> {
    fn is_closed(&self) -> bool {
        self.is_closed()
    }

    fn is_disconnected(&self) -> bool {
        #[allow(deprecated)]
        self.is_disconnected()
    }

    fn is_final(&self) -> bool {
        #[allow(deprecated)]
        self.is_final()
    }

    fn is_item_specific(&self) -> bool {
        false
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "channel is closed"),
            Self::Failed => write!(f, "send error"),
        }
    }
}

impl<T> From<mpsc::TrySendError<T>> for SendError<T> {
    fn from(err: mpsc::TrySendError<T>) -> Self {
        match err {
            mpsc::TrySendError::Closed(err) => Self::Closed(err),
            _ => Self::Failed,
        }
    }
}

impl<T> Error for SendError<T> where T: fmt::Debug {}

/// Sends a value to the associated receiver.
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "T: RemoteSend, Codec: codec::Codec"))]
#[serde(bound(deserialize = "T: RemoteSend, Codec: codec::Codec"))]
pub struct Sender<T, Codec = codec::Default>(pub(crate) mpsc::Sender<T, Codec, 1>);

impl<T, Codec> fmt::Debug for Sender<T, Codec> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T, Codec> Sender<T, Codec>
where
    T: RemoteSend,
    Codec: codec::Codec,
{
    /// Sends a value over this channel.
    pub fn send(self, value: T) -> Result<Sending<T>, SendError<T>> {
        self.0.try_send(value).map_err(|err| err.into())
    }

    /// Completes when the receiver has been closed, dropped or the connection failed.
    ///
    /// Use [closed_reason](Self::closed_reason) to obtain the cause for closure.
    pub async fn closed(&self) {
        self.0.closed().await
    }

    /// Returns the reason for why the channel has been closed.
    ///
    /// Returns [None] if the channel is not closed.
    pub fn closed_reason(&self) -> Option<ClosedReason> {
        self.0.closed_reason()
    }

    /// Returns whether the receiver has been closed, dropped or the connection failed.
    ///
    /// Use [closed_reason](Self::closed_reason) to obtain the cause for closure.
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// The maximum allowed item size in bytes.
    pub fn max_item_size(&self) -> usize {
        self.0.max_item_size()
    }

    /// Sets the maximum allowed item size in bytes.
    pub fn set_max_item_size(&mut self, max_item_size: usize) {
        self.0.set_max_item_size(max_item_size)
    }
}
