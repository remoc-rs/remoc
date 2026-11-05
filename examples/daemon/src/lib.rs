//! Shared interface for a local timer daemon and its command-line client.

#![warn(missing_docs)]

use remoc::prelude::*;
use std::time::Duration;

#[cfg(unix)]
#[path = "unix.rs"]
pub mod local;

#[cfg(windows)]
#[path = "windows.rs"]
pub mod local;

#[cfg(not(any(unix, windows)))]
compile_error!("This example requires Unix domain sockets or Windows named pipes.");

/// A reminder scheduled by the daemon.
///
/// This ordinary serializable data is used both in RPC results and channel messages.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Timer {
    /// Identifier assigned by the daemon.
    pub id: u64,
    /// Message to display when the timer finishes.
    pub message: String,
}

/// A reminder to schedule, optionally with its own completion channel.
///
/// Remoc channel endpoints implement Serde too: the sender travels inside this
/// request while its receiver stays with the client. No extra socket is needed.
/// A channel endpoint could also be passed directly as an RPC argument instead.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct NewTimer {
    /// Delay before the reminder finishes.
    pub after: Duration,
    /// Message to display when the timer finishes.
    pub message: String,
    /// An optional channel to notify when this timer finishes.
    pub notify: Option<rch::oneshot::Sender<Timer>>,
}

/// Scheduling a timer failed.
///
/// Application errors travel back as RPC results. `Call` also represents failures
/// to perform the call itself, such as a disconnected daemon.
#[derive(Debug, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum AddError {
    /// The delay cannot be represented by the daemon's clock.
    #[error("the requested delay is too long")]
    InvalidDelay,
    /// All timer identifiers have been used.
    #[error("no timer identifiers remain")]
    IdExhausted,
    /// The remote call failed.
    #[error(transparent)]
    Call(#[from] rtc::CallError),
}

/// Timers belong to the daemon, not to the client that scheduled them.
///
/// The macro generates [`TimerServiceClient`] for making remote calls and
/// [`TimerServiceServerShared`] for dispatching them to our implementation.
/// Calling a client method sends its arguments to the daemon and awaits its result.
///
/// `Shared` selects a server backed by `Arc<T>`: callers share one object,
/// which handles its own synchronization.
#[rtc::remote(server(Shared))]
pub trait TimerService {
    /// Schedule a reminder and return its identifier, without waiting for expiry.
    async fn add(&self, timer: NewTimer) -> Result<u64, AddError>;

    /// List reminders that have not finished yet.
    async fn list(&self) -> Result<Vec<Timer>, rtc::CallError>;

    /// Receive future timer completions over the existing connection.
    ///
    /// The result is a live channel endpoint, not a batch of events. Completions
    /// continue arriving on the receiver after this RPC call has returned.
    async fn subscribe(&self) -> Result<rch::broadcast::Receiver<Timer>, rtc::CallError>;
}
