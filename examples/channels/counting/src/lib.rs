//! This library crate defines the data exchanged by the counting example.
//!
//! The client and server depend on it. It is the whole contract between
//! them: there is no schema file and nothing to generate.
#![warn(missing_docs)]

use remoc::prelude::*;

/// TCP port the server is listening on.
pub const TCP_PORT: u16 = 9870;

/// A request to count up to a number.
///
/// Remoc types such as channel senders and receivers are serializable, so they
/// can be placed in a struct like any other field. Sending this request creates
/// the channel `seq_tx` belongs to inside the connection that carries the
/// request, which is what saves the client from opening a second connection or
/// agreeing on an identifier with the server.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CountReq {
    /// Count up to, but excluding, this number.
    pub up_to: u32,
    /// The sender the server should count into.
    ///
    /// The client keeps the matching receiver.
    pub seq_tx: rch::mpsc::Sender<u32>,
}
