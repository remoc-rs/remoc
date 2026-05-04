//! A channel for streaming binary data with receive verification.
//!
//! This channel provides [`AsyncWrite`](tokio::io::AsyncWrite) and
//! [`AsyncRead`](tokio::io::AsyncRead) implementations for streaming binary data
//! between endpoints. The receiver can verify that all transmitted data has been
//! received completely, ensuring data integrity.
//!
//! # Modes
//!
//! The channel supports two modes depending on whether the total data size is known upfront:
//!
//! - **Known size** ([`sized`]): The sender specifies the exact number of bytes to transmit.
//!   The receiver knows the size in advance via [`Receiver::size`] and verifies that the
//!   received data matches.
//!
//! - **Unknown size** ([`channel`]): The sender can write any amount of data.
//!   The receiver learns the final size only upon completion and verifies integrity.
//!
//! In both cases, the receiver returns an error if the received byte count does not match
//! the expected size.
//!
//! # Completion
//!
//! - **Known size**: Calling [`shutdown`](tokio::io::AsyncWriteExt::shutdown) is optional.
//!   The receiver uses the pre-announced size to determine completion.
//!   However, [`flush`](tokio::io::AsyncWriteExt::flush) must be called before dropping
//!   the sender to ensure all pending data is transmitted, as per the standard
//!   [`AsyncWrite`](tokio::io::AsyncWrite) contract.
//!
//! - **Unknown size**: Calling [`shutdown`](tokio::io::AsyncWriteExt::shutdown) is **required**.
//!   This sends the final byte count to the receiver. If the sender is dropped without calling
//!   shutdown, the receiver will return an [`UnexpectedEof`](std::io::ErrorKind::UnexpectedEof) error.
//!
//! # Local and remote use
//!
//! Both halves of this channel can be used locally without sending either to a remote endpoint.
//! In that case an in-process loopback connection is used automatically.
//! Forwarding, i.e. passing channel ends through intermediate remote endpoints, is also supported.
//!
//! # Example: Known size
//!
//! Use [`sized`] when the data size is known upfront (e.g., file transfer with known file size).
//!
//! ```
//! use remoc::prelude::*;
//! use tokio::io::{AsyncWriteExt, AsyncReadExt};
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<rch::io::Receiver>) {
//!     let (mut io_tx, io_rx) = rch::io::sized(11);
//!
//!     // The sender knows the expected size.
//!     assert_eq!(io_tx.expected_size(), Some(11));
//!
//!     // Send receiver to server.
//!     tx.send(io_rx).await.unwrap();
//!
//!     // Write data.
//!     io_tx.write_all(b"hello world").await.unwrap();
//!     io_tx.shutdown().await.unwrap();
//!     // For sized channels, this would be sufficient:
//!     // io_tx.flush().await.unwrap();
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<rch::io::Receiver>) {
//!     let mut io_rx = rx.recv().await.unwrap().unwrap();
//!
//!     // The receiver knows the size in advance.
//!     assert_eq!(io_rx.size(), Some(11));
//!
//!     // Read data.
//!     let mut buf = Vec::new();
//!     io_rx.read_to_end(&mut buf).await.unwrap();
//!     assert_eq!(buf, b"hello world");
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```
//!
//! # Example: Unknown size
//!
//! Use [`channel`] when the data size is not known upfront (e.g., streaming or compressed data).
//! **Calling shutdown is required** to signal completion and send the final size to the receiver.
//!
//! ```
//! use remoc::prelude::*;
//! use tokio::io::{AsyncWriteExt, AsyncReadExt};
//!
//! // This would be run on the client.
//! async fn client(mut tx: rch::base::Sender<rch::io::Receiver>) {
//!     let (mut io_tx, io_rx) = rch::io::channel();
//!
//!     // Size is unknown.
//!     assert_eq!(io_tx.expected_size(), None);
//!
//!     // Send receiver to server.
//!     tx.send(io_rx).await.unwrap();
//!
//!     // Write data in chunks (size determined at runtime).
//!     io_tx.write_all(b"streaming ").await.unwrap();
//!     io_tx.write_all(b"data").await.unwrap();
//!
//!     // REQUIRED: shutdown sends the final size to the receiver.
//!     // Without this, the receiver will fail with UnexpectedEof.
//!     io_tx.shutdown().await.unwrap();
//! }
//!
//! // This would be run on the server.
//! async fn server(mut rx: rch::base::Receiver<rch::io::Receiver>) {
//!     let mut io_rx = rx.recv().await.unwrap().unwrap();
//!
//!     // Size is unknown until EOF.
//!     assert_eq!(io_rx.size(), None);
//!
//!     // Read all data.
//!     let mut buf = Vec::new();
//!     io_rx.read_to_end(&mut buf).await.unwrap();
//!     assert_eq!(buf, b"streaming data");
//!
//!     // After EOF, size becomes known.
//!     assert_eq!(io_rx.size(), Some(14));
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(client, server));
//! ```

use serde::{Deserialize, Serialize};

use super::{bin, oneshot};
use crate::codec;

mod receiver;
mod sender;

pub use receiver::Receiver;
pub use sender::Sender;

use sender::SizeMode;

/// Internal enum to track size information on the receiver side.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "Codec: codec::Codec"))]
#[serde(bound(deserialize = "Codec: codec::Codec"))]
pub(super) enum SizeInfo<Codec> {
    /// Size was known at channel creation.
    Determined(u64),
    /// Size will be received when sender shuts down.
    Undetermined(oneshot::Receiver<u64, Codec>),
}

/// Creates a new I/O channel with unknown size.
///
/// The sender must call [`AsyncWriteExt::shutdown`](tokio::io::AsyncWriteExt::shutdown)
/// when done writing to signal completion.
/// The receiver cannot know the size in advance (returns `None` from [`Receiver::size`]).
///
/// Both ends can be sent to remote endpoints.
pub fn channel<Codec>() -> (Sender<Codec>, Receiver<Codec>)
where
    Codec: codec::Codec,
{
    let (bin_tx, bin_rx) = bin::channel();
    let (size_tx, size_rx) = oneshot::channel();

    let sender = Sender::new(bin_tx, SizeMode::Unknown(size_tx));
    let receiver = Receiver::new(bin_rx, SizeInfo::Undetermined(size_rx));

    (sender, receiver)
}

/// Creates a new I/O channel with known size.
///
/// The sender is expected to write exactly `size` bytes.
/// Attempting to write more will result in an error.
/// Calling [`flush`](tokio::io::AsyncWriteExt::flush) before dropping ensures all data is sent.
/// If [`shutdown`](tokio::io::AsyncWriteExt::shutdown) is called, it verifies that exactly
/// `size` bytes were written.
///
/// The receiver can query the size via [`Receiver::size`], which returns `Some(size)`.
///
/// Both ends can be sent to remote endpoints.
pub fn sized<Codec>(size: u64) -> (Sender<Codec>, Receiver<Codec>)
where
    Codec: codec::Codec,
{
    let (bin_tx, bin_rx) = bin::channel();

    let sender = Sender::new(bin_tx, SizeMode::Known(size));
    let receiver = Receiver::new(bin_rx, SizeInfo::Determined(size));

    (sender, receiver)
}
