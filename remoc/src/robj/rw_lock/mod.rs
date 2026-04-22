//! A read/write lock that shares a consistent view of a value over multiple remote endpoints.
//!
//! A remote read/write lock allows multiple remote endpoints to share a common value
//! that can be read and written by each endpoint.
//! Multiple endpoints can acquire simultaneous read access to the value.
//! Write access is exclusive and can only be held by one endpoint at a time.
//! This ensures that all endpoints always have a consist view of the shared value.
//! The shared value is always stored on the endpoint that created the
//! [RwLock owner](Owner).
//!
//! Each [RwLock] caches the current value locally until it is invalidated.
//! Thus, subsequent read operations are cheap.
//! However, a write operation requires several round trips between the owner
//! and the remote endpoints, thus its performance is limited by the physical connection
//! latency.
//!
//! # Security
//!
//! This module is **not** designed for use with untrusted remote peers.
//! A malicious peer can hold a [ReadGuard] or [WriteGuard]
//! indefinitely, which blocks the [Owner] from processing any further
//! read or write requests, effectively causing a denial of service for all peers.
//! Only use this module with trusted peers.
//!
//! # Usage
//!
//! [Create an RwLock owner](Owner::new) and use [Owner::rw_lock] to acquire
//! read/write locks that can be send to remote endpoints, for example over a
//! [remote channel](crate::rch).
//! The remote endpoints can then use [RwLock::read] and [RwLock::write] to obtain
//! read or write access respectively.
//! When the [owner](Owner) is dropped, all locks become invalid and the value
//! is dropped.
//!
//! # Alternatives
//!
//! If you require to broadcast value to multiple endpoints that just require read
//! access, a [watch channel](crate::rch::watch) might be a simpler and better option.
//!
//! # Example
//!
//! In the following example the server creates a lock owner and obtains a
//! read/write lock from it that is sent to the client.
//! The client obtains a read guard and verifies the data.
//! Then it obtains a write guard and changes the data.
//! Afterwards it obtains a new read guard and verifies that the changes have
//! been performed.
//!
//! ```
//! use remoc::prelude::*;
//! use remoc::robj::rw_lock::{Owner, RwLock};
//!
//! #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
//! struct Data {
//!     field1: u32,
//!     field2: String,
//! }
//!
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<RwLock<Data>>) {
//!     let mut rw_lock = rx.recv().await.unwrap().unwrap();
//!     
//!     let read = rw_lock.read().await.unwrap();
//!     assert_eq!(read.field1, 123);
//!     assert_eq!(read.field2, "data");
//!     drop(read);
//!
//!     let mut write = rw_lock.write().await.unwrap();
//!     write.field1 = 222;
//!     write.commit().await.unwrap();
//!
//!     let read = rw_lock.read().await.unwrap();
//!     assert_eq!(read.field1, 222);
//! }
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<RwLock<Data>>) {
//!     let data = Data { field1: 123, field2: "data".to_string() };
//!     let owner = Owner::new(data);
//!     tx.send(owner.rw_lock()).await.unwrap();
//!
//!     // The owner must be kept alive until the client is done with the lock.
//!     tx.closed().await;
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```
//!

mod msg;
mod owner;
#[allow(clippy::module_inception)]
mod rw_lock;

pub use owner::Owner;
pub use rw_lock::{CommitError, LockError, ReadGuard, ReadLock, RwLock, WriteGuard};
