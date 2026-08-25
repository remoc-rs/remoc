//! The codec that is currently serializing.

use std::cell::Cell;

/// Properties of the codec that is currently serializing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveCodec {
    /// The name of the codec.
    pub name: &'static str,
    /// Whether the receiving endpoint can decode data that leaves out a struct field.
    pub allow_skip: bool,
}

thread_local! {
    static ACTIVE: Cell<Option<ActiveCodec>> = const { Cell::new(None) };
}

/// Restores the previously active codec when dropped.
#[must_use = "the codec is only active while the guard is alive"]
#[derive(Debug)]
pub(crate) struct ActiveGuard(Option<ActiveCodec>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(self.0.take()));
    }
}

/// Makes the specified codec active until the returned guard is dropped.
pub(crate) fn activate(codec: ActiveCodec) -> ActiveGuard {
    ActiveGuard(ACTIVE.with(|active| active.replace(Some(codec))))
}

/// The codec that is currently serializing, if any.
pub(crate) fn active() -> Option<ActiveCodec> {
    ACTIVE.with(|active| active.get())
}
