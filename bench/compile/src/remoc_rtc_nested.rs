// Compile-time benchmark: nested remote traits.
//
// Every level defines a remote trait whose methods return the client of the level
// below and remote objects. Proving the generated client and server futures `Send`
// unfolds the request and response types of all nested levels, so this benchmark is
// dominated by trait solving during type checking rather than by code generation.
//
// Without the `explicit-auto-traits` feature of remoc the proofs exceed the default
// recursion limit, so it is raised here as users of deeply nested remote traits do.
#![recursion_limit = "1024"]

use bytes::Bytes;
use remoc::{
    prelude::*,
    robj::{lazy::Lazy, lazy_blob::LazyBlob},
    rtc::{self, CallError},
};
use std::sync::Arc;

/// Defines one level: the remote trait, an implementation and the constructor
/// of the client for the level below.
macro_rules! level {
    ($trait:ident, $imp:ident, $next_client:ty, $next:expr) => {
        #[rtc::remote(server(Shared))]
        pub trait $trait {
            async fn next(&self) -> Result<$next_client, CallError>;
            async fn blob(&self) -> Result<LazyBlob, CallError>;
            async fn lazy(&self) -> Result<Lazy<Vec<u8>>, CallError>;
            async fn watch(&self) -> Result<rch::watch::Receiver<u64>, CallError>;
            async fn describe(&self, name: String, count: u32) -> Result<String, CallError>;
        }

        pub struct $imp;

        impl $trait for $imp {
            async fn next(&self) -> Result<$next_client, CallError> {
                Ok($next)
            }

            async fn blob(&self) -> Result<LazyBlob, CallError> {
                Ok(LazyBlob::new(Bytes::from_static(b"data")))
            }

            async fn lazy(&self) -> Result<Lazy<Vec<u8>>, CallError> {
                Ok(Lazy::new(vec![1, 2, 3]))
            }

            async fn watch(&self) -> Result<rch::watch::Receiver<u64>, CallError> {
                let (_tx, rx) = rch::watch::channel(0);
                Ok(rx)
            }

            async fn describe(&self, name: String, count: u32) -> Result<String, CallError> {
                Ok(format!("{name}: {count}"))
            }
        }
    };
}

/// Creates a served client of the given level.
macro_rules! serve {
    ($server:ident, $imp:ident) => {{
        let (server, client) = $server::new(Arc::new($imp));
        tokio::spawn(server.serve());
        client
    }};
}

level!(L0, L0Impl, u64, 0);
level!(L1, L1Impl, L0Client, serve!(L0ServerShared, L0Impl));
level!(L2, L2Impl, L1Client, serve!(L1ServerShared, L1Impl));
level!(L3, L3Impl, L2Client, serve!(L2ServerShared, L2Impl));
level!(L4, L4Impl, L3Client, serve!(L3ServerShared, L3Impl));
level!(L5, L5Impl, L4Client, serve!(L4ServerShared, L4Impl));
level!(L6, L6Impl, L5Client, serve!(L5ServerShared, L5Impl));
level!(L7, L7Impl, L6Client, serve!(L6ServerShared, L6Impl));

#[tokio::main]
async fn main() {
    let l7: L7Client = serve!(L7ServerShared, L7Impl);
    let l6 = l7.next().await.unwrap();
    let l5 = l6.next().await.unwrap();
    let l4 = l5.next().await.unwrap();
    let l3 = l4.next().await.unwrap();
    let l2 = l3.next().await.unwrap();
    let l1 = l2.next().await.unwrap();
    let l0 = l1.next().await.unwrap();

    assert_eq!(l0.next().await.unwrap(), 0);
    assert_eq!(l0.describe("level".into(), 0).await.unwrap(), "level: 0");
    let _ = l7.blob().await.unwrap();
    let _ = l4.lazy().await.unwrap();
    let _ = l1.watch().await.unwrap();
}
