//! Remote objects.
//!
//! Remote objects represent data whose lifetime or access spans endpoints. They
//! can be sent over [remote channels](crate::rch), passed to
//! [remote functions](crate::rfn), and used in [remote traits](crate::rtc).
//!
//! # Choosing an object type
//!
//! | Need | Type |
//! |---|---|
//! | Send an opaque identity away and recognize it when it returns | [`handle::Handle`] |
//! | Transfer a value only if the receiver asks for it | [`lazy::Lazy`] |
//! | Lazily transfer a potentially large byte buffer | [`lazy_blob::LazyBlob`] |
//! | Share readable and writable state between trusted endpoints | [`rw_lock::RwLock`] |
//!
//! A [`handle::Handle`] does not provide remote access to its value: it can only
//! be dereferenced on the endpoint where the value is stored. In contrast,
//! [`rw_lock::RwLock`] coordinates actual access across endpoints and therefore
//! incurs network round trips.
//!
//! Lazy values trade bandwidth for latency. Sending one is cheap, but the first
//! request for its contents requires another round trip. Use
//! [`lazy_blob::LazyBlob`] instead of [`lazy::Lazy`] for raw binary data so it can
//! be transferred without a Serde codec.
//!
//! # Lifetime control
//!
//! Several remote objects offer a `provided` constructor returning a provider.
//! Keeping the provider gives the creating endpoint explicit control over the
//! object's lifetime. Dropping it makes the object unavailable even if a remote
//! peer retained a reference, which is important when peers are not trusted to
//! release resources promptly.
//!

pub mod handle;
pub mod lazy;
pub mod lazy_blob;
pub mod rw_lock;
