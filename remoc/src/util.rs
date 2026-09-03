//! Utility functions.

use bytes::{Buf, Bytes};
use std::fmt;

/// Debug formatter for [Bytes].
pub fn dbg_bytes(bytes: &Bytes, f: &mut fmt::Formatter) -> fmt::Result {
    const LIMIT: usize = 16;

    if bytes.len() > LIMIT {
        let tmp = bytes.clone().copy_to_bytes(LIMIT);
        write!(f, "{tmp:?}...[{} bytes]", bytes.len())
    } else {
        write!(f, "{bytes:?}")
    }
}

/// Debug formatter for `Option<Bytes>`.
pub fn dbg_option_bytes(bytes: &Option<Bytes>, f: &mut fmt::Formatter) -> fmt::Result {
    match bytes {
        Some(bytes) => {
            write!(f, "Some(")?;
            dbg_bytes(bytes, f)?;
            write!(f, ")")?;
        }
        None => write!(f, "None")?,
    }
    Ok(())
}

/// Creates the span of a task that outlives the operation spawning it.
///
/// Level, name and fields are specified as for [`span!`](tracing::span).
macro_rules! task_span {
    ($level:expr, $name:literal $(, $($fields:tt)*)?) => {{
        let span = ::tracing::span!($level, $name $(, $($fields)*)?);
        span.follows_from(::tracing::Span::current());
        span
    }};
}
pub(crate) use task_span;
