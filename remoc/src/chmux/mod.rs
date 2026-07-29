//! Low-level channel multiplexer.
//!
//! Multiplexes multiple binary channels over a single binary channel
//! (anything that implements [Sink](futures::Sink) and [Stream](futures::Stream)).
//! A connection is established by calling [ChMux::new].
//!
//! **You probably do not want to use this module directly.**
//! Instead use methods from [Connect](crate::Connect) to establish a connection over
//! a physical transport and work with high-level [remote channels](crate::rch).
//!
//! # Protocol version compatibility
//! Two endpoints can only communicate if they have the same [protocol version](PROTOCOL_VERSION).
//! A change in protocol version will be accompanied by an increase of the
//! major version number of the Remoc crate.

use std::{error::Error, fmt};

mod any_storage;
mod cfg;
mod client;
mod credit;
mod forward;
mod listener;
mod msg;
mod mux;
mod port_allocator;
mod receiver;
mod sender;
mod sizer;

pub use any_storage::{AnyBox, AnyEntry, AnyStorage};
pub use cfg::{Cfg, PortsExhausted};
pub use client::{Client, Connect, ConnectError};
pub use forward::ForwardError;
pub use listener::{Listener, ListenerError, ListenerStream, Request};
pub use mux::ChMux;
pub use port_allocator::{PortAllocator, PortNumber, PortReq};
pub use receiver::{DataBuf, Received, Receiver, ReceiverStream, RecvAnyError, RecvChunkError, RecvError};
pub use sender::{AllReceived, ChunkSender, Closed, SendError, Sender, SenderSink, TrySendError};
pub use sizer::{BufferSize, BufferSizeQuery, BufferSizer, DynamicBuffer, FixedBuffer, GlobalCreditsReport};

/// Channel multiplexer protocol version.
pub const PROTOCOL_VERSION: u8 = 3;

/// Lowest protocol version that supports port ids.
const PROTOCOL_VERSION_PORT_ID: u8 = 3;

/// Channel multiplexer error.
#[derive(Debug, Clone)]
pub enum ChMuxError<SinkError, StreamError> {
    /// An error was encountered while sending data to the transport sink.
    SinkError(SinkError),
    /// An error was encountered while receiving data from the transport stream.
    StreamError(StreamError),
    /// The transport stream was closed while multiplex channels were active or the
    /// multiplex client was not dropped.
    StreamClosed,
    /// The connection was reset by the remote endpoint.
    Reset,
    /// No messages where received over the configured connection timeout.
    Timeout,
    /// A multiplex protocol error occurred.
    Protocol(String),
}

impl<SinkError, StreamError> fmt::Display for ChMuxError<SinkError, StreamError>
where
    SinkError: fmt::Display,
    StreamError: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SinkError(err) => write!(f, "send error: {err}"),
            Self::StreamError(err) => write!(f, "receive error: {err}"),
            Self::StreamClosed => write!(f, "end of receive stream"),
            Self::Reset => write!(f, "connection reset"),
            Self::Timeout => write!(f, "connection timeout"),
            Self::Protocol(err) => write!(f, "protocol error: {err}"),
        }
    }
}

impl<SinkError, StreamError> Error for ChMuxError<SinkError, StreamError>
where
    SinkError: Error,
    StreamError: Error,
{
}

impl From<ChMuxError<std::io::Error, std::io::Error>> for std::io::Error {
    fn from(err: ChMuxError<std::io::Error, std::io::Error>) -> Self {
        use std::io::ErrorKind;
        match err {
            ChMuxError::SinkError(err) => err,
            ChMuxError::StreamError(err) => err,
            ChMuxError::StreamClosed => std::io::Error::new(ErrorKind::ConnectionReset, err.to_string()),
            ChMuxError::Reset => std::io::Error::new(ErrorKind::ConnectionReset, err.to_string()),
            ChMuxError::Timeout => std::io::Error::new(ErrorKind::TimedOut, err.to_string()),
            ChMuxError::Protocol(_) => std::io::Error::new(ErrorKind::InvalidData, err.to_string()),
        }
    }
}
