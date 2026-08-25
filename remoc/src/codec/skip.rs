//! Leaving struct fields out of the serialized data.
//!
//! Some, but not all, data formats can leave a struct field out, so that the receiving
//! endpoint takes its `#[serde(default)]` value.
//!
//! Whether a field may be left out is thus a property of the employed codec and capabilities
//! of the remote endpoint. The predicates here decide it when the data is actually serialized:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! #[derive(Serialize, Deserialize)]
//! struct Request {
//!     value: u32,
//!     #[serde(default, skip_serializing_if = "remoc::codec::skip::if_default")]
//!     verbose: bool,
//! }
//! ```
//!

use super::active;

/// Whether the codec that is serializing may leave a struct field out of the data.
///
/// This is `true` when no Remoc codec is serializing.
pub fn allow_skip() -> bool {
    active::active().is_none_or(|codec| codec.allow_skip)
}

/// Whether the field may be left out because it holds its default value.
///
/// Use as `#[serde(default, skip_serializing_if = "remoc::codec::skip::if_default")]`.
pub fn if_default<T>(value: &T) -> bool
where
    T: Default + PartialEq,
{
    allow_skip() && *value == T::default()
}

/// Predicates for [`Option`](std::option::Option).
pub enum Option {}

impl Option {
    /// Whether the field may be left out because it is [`None`].
    pub fn is_none<T>(value: &std::option::Option<T>) -> bool {
        allow_skip() && value.is_none()
    }
}
