//! Messages exchanged between remote functions and their providers.

use crate::{RemoteSend, codec, rch::oneshot, tracing::TracingContext};

/// Remote function call request.
pub struct RFnRequest<A, R, Codec> {
    /// Function argument.
    pub argument: A,
    /// Channel for result transmission.
    pub result_tx: oneshot::Sender<R, Codec>,
    /// Tracing context of the caller.
    pub tracing: Option<TracingContext>,
}

crate::versioned::compact::impl_struct! {
    RFnRequest<A, R, Codec>,
    fields {
        argument: A => "_0",
        result_tx: oneshot::Sender<R, Codec> => "_1",
        #[serde(default)]
        #[serde(skip_serializing_if = "crate::codec::skip::Option::is_none")]
        tracing: Option<TracingContext> => "_2",
    }
    where A: RemoteSend, R: RemoteSend, Codec: codec::Codec
}
