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
//! # Opening ports
//!
//! Each port is a bidirectional binary channel represented by a [`Sender`] and
//! [`Receiver`]. [`ChMux::new`] returns a [`Client`] for opening ports and a
//! [`Listener`] for accepting them:
//!
//! - [`Client::connect_port`] is the convenient way to open a port. The remote
//!   endpoint receives it through [`Listener::accept`].
//! - For more control, create a [`ConnectReq`] with [`Client::connect_req`],
//!   configure it, and pass it to [`Client::connect`].
//! - A port can itself carry requests for additional ports. Use
//!   [`Sender::connect_req`] and [`Sender::connect`]; the peer receives
//!   [`Received::Requests`] from [`Receiver::recv_any`] and can
//!   [`accept`](Request::accept) or [`reject`](Request::reject) each request.
//!
//! A [`pre-connected`](ConnectReq::pre_connect) port is provisionally opened so
//! data can flow while the remote listener's acceptance is still pending. This
//! avoids waiting for a round trip before sending the first data, but the listener
//! can still reject the connection. [`Client::connect_port`] attempts this
//! optimization automatically.
//!
//! The high-level [`rch`](crate::rch) channels use these mechanisms internally,
//! including when channels are sent inside other values. Applications using
//! `rch` do not need to open or pre-connect ports themselves.
//!
//! Data is sent in chunks and every port is subject to credit-based flow control, which is
//! what provides [back pressure](crate::rch#flow-control-and-back-pressure) to the remote
//! channels built on top of this module.
//! [Cfg] configures the buffers involved.
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
pub use cfg::{Cfg, OnPortsExhausted};
pub use client::{Client, Connect, ConnectError};
pub use forward::ForwardError;
pub use listener::{Listener, ListenerError, ListenerStream, Request};
pub use mux::ChMux;
pub use port_allocator::{ConnectReq, PortAllocator, PortsExhausted, SidePort};
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
    /// No messages were received within the configured connection timeout.
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
            ChMuxError::StreamClosed => std::io::Error::new(ErrorKind::ConnectionReset, err),
            ChMuxError::Reset => std::io::Error::new(ErrorKind::ConnectionReset, err),
            ChMuxError::Timeout => std::io::Error::new(ErrorKind::TimedOut, err),
            ChMuxError::Protocol(_) => std::io::Error::new(ErrorKind::InvalidData, err),
        }
    }
}
