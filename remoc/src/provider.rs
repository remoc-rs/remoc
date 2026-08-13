//! Provider for any remote object.

#[cfg(feature = "rfn")]
use crate::rfn;

#[cfg(feature = "robj")]
use crate::robj;

/// A provider for any remote object.
///
/// This helps to store providers of different remote object types together.
/// For example, you can keep providers of different types in a single vector.
///
/// Dropping the provider will stop making the object available remotely.
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
    /// Releases the provider while keeping the remote object alive
    /// until it is dropped.
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

    /// Waits until the provider can be safely dropped because it is not
    /// needed anymore.
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
