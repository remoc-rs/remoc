//! Type-erased ownership of remote object providers.

#[cfg(feature = "rfn")]
use crate::rfn;

#[cfg(feature = "robj")]
use crate::robj;

/// A provider for any remote object.
///
/// This enum allows providers of different remote object types to be stored
/// together, for example in a single collection.
///
/// Dropping a provider stops serving its remote object. Existing users observe
/// the object as unavailable; use [`done`](Self::done) before dropping when the
/// object should remain available until all remote references are gone.
#[derive(Debug)]
#[non_exhaustive]
pub enum Provider {
    /// A provider for [RFn](rfn::RFn).
    #[cfg(feature = "rfn")]
    RFn(rfn::RFnProvider),
    /// A provider for [RFnMut](rfn::RFnMut).
    #[cfg(feature = "rfn")]
    RFnMut(rfn::RFnMutProvider),
    /// A provider for [RFnOnce](rfn::RFnOnce).
    #[cfg(feature = "rfn")]
    RFnOnce(rfn::RFnOnceProvider),
    /// A provider for [Handle](robj::handle::Handle).
    #[cfg(feature = "robj")]
    Handle(robj::handle::Provider),
    /// A provider for [Lazy](robj::lazy::Lazy).
    #[cfg(feature = "robj")]
    Lazy(robj::lazy::Provider),
    /// A provider for [LazyBlob](robj::lazy_blob::LazyBlob).
    #[cfg(feature = "robj")]
    LazyBlob(robj::lazy_blob::Provider),
}

#[cfg(feature = "rfn")]
impl From<rfn::RFnProvider> for Provider {
    fn from(provider: rfn::RFnProvider) -> Self {
        Self::RFn(provider)
    }
}

#[cfg(feature = "rfn")]
impl From<rfn::RFnMutProvider> for Provider {
    fn from(provider: rfn::RFnMutProvider) -> Self {
        Self::RFnMut(provider)
    }
}

#[cfg(feature = "rfn")]
impl From<rfn::RFnOnceProvider> for Provider {
    fn from(provider: rfn::RFnOnceProvider) -> Self {
        Self::RFnOnce(provider)
    }
}

#[cfg(feature = "robj")]
impl From<robj::handle::Provider> for Provider {
    fn from(provider: robj::handle::Provider) -> Self {
        Self::Handle(provider)
    }
}

#[cfg(feature = "robj")]
impl From<robj::lazy::Provider> for Provider {
    fn from(provider: robj::lazy::Provider) -> Self {
        Self::Lazy(provider)
    }
}

#[cfg(feature = "robj")]
impl From<robj::lazy_blob::Provider> for Provider {
    fn from(provider: robj::lazy_blob::Provider) -> Self {
        Self::LazyBlob(provider)
    }
}

impl Provider {
    /// Keeps serving the remote object without retaining a provider handle.
    ///
    /// After this call the object remains available until all remote references
    /// to it have been dropped. This operation cannot be undone.
    pub fn keep(self) {
        match self {
            #[cfg(feature = "rfn")]
            Self::RFn(provider) => provider.keep(),
            #[cfg(feature = "rfn")]
            Self::RFnMut(provider) => provider.keep(),
            #[cfg(feature = "rfn")]
            Self::RFnOnce(provider) => provider.keep(),
            #[cfg(feature = "robj")]
            Self::Handle(provider) => provider.keep(),
            #[cfg(feature = "robj")]
            Self::Lazy(provider) => provider.keep(),
            #[cfg(feature = "robj")]
            Self::LazyBlob(provider) => provider.keep(),
        }
    }

    /// Waits until no remote references need this provider.
    ///
    /// The provider continues serving while this method waits. Once it returns,
    /// dropping the provider does not interrupt a remote user.
    pub async fn done(&mut self) {
        match self {
            #[cfg(feature = "rfn")]
            Self::RFn(provider) => provider.done().await,
            #[cfg(feature = "rfn")]
            Self::RFnMut(provider) => provider.done().await,
            #[cfg(feature = "rfn")]
            Self::RFnOnce(provider) => provider.done().await,
            #[cfg(feature = "robj")]
            Self::Handle(provider) => provider.done().await,
            #[cfg(feature = "robj")]
            Self::Lazy(provider) => provider.done().await,
            #[cfg(feature = "robj")]
            Self::LazyBlob(provider) => provider.done().await,
        }
    }
}
