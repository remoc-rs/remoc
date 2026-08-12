//! Messages exchanged between remote functions and their providers.

use crate::{RemoteSend, codec, rch::oneshot};

/// Remote function call request.
pub struct RFnRequest<A, R, Codec> {
    /// Function argument.
    pub argument: A,
    /// Channel for result transmission.
    pub result_tx: oneshot::Sender<R, Codec>,
}

crate::versioned::compact::impl_struct! {
    RFnRequest<A, R, Codec>,
    fields {
        argument: A => "_0",
        result_tx: oneshot::Sender<R, Codec> => "_1",
    }
    where A: RemoteSend, R: RemoteSend, Codec: codec::Codec
}
