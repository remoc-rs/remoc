//! Remote trait shared by the web counter client and server.
#![warn(missing_docs)]

use remoc::prelude::*;
use std::{error::Error, fmt};

/// HTTP and WebSocket port used by the server.
pub const HTTP_PORT: u16 = 9872;

/// Changing the counter failed.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum ChangeError {
    /// The value cannot be increased any further.
    Maximum,
    /// The value cannot be decreased any further.
    Minimum,
    /// The RTC call failed.
    Call(rtc::CallError),
}

impl From<rtc::CallError> for ChangeError {
    fn from(error: rtc::CallError) -> Self {
        Self::Call(error)
    }
}

impl fmt::Display for ChangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Maximum => formatter.write_str("the counter reached its maximum value"),
            Self::Minimum => formatter.write_str("the counter cannot be decreased below zero"),
            Self::Call(error) => error.fmt(formatter),
        }
    }
}

impl Error for ChangeError {}

/// A counter shared by all connected clients.
// The macro generates the CounterClient and CounterServerSharedMut types.
#[rtc::remote(server(SharedMut))]
pub trait Counter {
    /// Increase the value by one.
    async fn increment(&mut self) -> Result<(), ChangeError>;

    /// Decrease the value by one.
    async fn decrement(&mut self) -> Result<(), ChangeError>;

    /// Subscribe to the current value and subsequent changes.
    async fn watch(&self) -> Result<rch::watch::Receiver<u32>, rtc::CallError>;
}
