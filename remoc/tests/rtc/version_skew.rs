//! A client and a server that were built against different versions of a trait.
//!
//! Calling a method the server does not know must fail that call alone. The
//! [monitors installed by default](remoc::rtc::monitor#default-monitors) skip the
//! request the server cannot decode, so that serving continues.

use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::RwLock;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{Cfg, Connect, rtc::ServerSharedMut};

/// The trait as the server knows it.
mod older {
    #[remoc::rtc::remote]
    pub trait Counter {
        async fn value(&self) -> Result<u32, remoc::rtc::CallError>;
        async fn increase(&mut self, by: u32) -> Result<u32, remoc::rtc::CallError>;
    }

    #[derive(Default)]
    pub struct CounterObj {
        pub value: u32,
    }

    impl Counter for CounterObj {
        async fn value(&self) -> Result<u32, remoc::rtc::CallError> {
            Ok(self.value)
        }

        async fn increase(&mut self, by: u32) -> Result<u32, remoc::rtc::CallError> {
            self.value += by;
            Ok(self.value)
        }
    }
}

/// The same trait, with a method added by a later version.
mod newer {
    #[remoc::rtc::remote]
    pub trait Counter {
        async fn value(&self) -> Result<u32, remoc::rtc::CallError>;
        async fn increase(&mut self, by: u32) -> Result<u32, remoc::rtc::CallError>;
        async fn added_later(&self) -> Result<u32, remoc::rtc::CallError>;
    }
}

/// A client calling a method the server does not know fails that call, but keeps
/// the server serving the methods it does know.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unknown_method_does_not_end_serving() {
    use newer::Counter as _;

    crate::init();
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let server_side = Connect::framed::<_, _, _, _, older::CounterClient, (), remoc::codec::Default>(
        Cfg::default(),
        a_tx,
        a_rx,
    );
    let client_side = Connect::framed::<_, _, _, _, (), newer::CounterClient, remoc::codec::Default>(
        Cfg::default(),
        b_tx,
        b_rx,
    );
    let ((server_conn, mut server_tx, _), (client_conn, _, mut client_rx)) =
        futures::future::try_join(server_side, client_side).await.unwrap();
    wokio::spawn(async move {
        let _ = server_conn.await;
    });
    wokio::spawn(async move {
        let _ = client_conn.await;
    });

    // The server is left with the monitors that are installed by default.
    let obj = Arc::new(RwLock::new(older::CounterObj::default()));
    let (server, client) = older::CounterServerSharedMut::new(obj.clone());
    wokio::spawn(async move {
        let _ = server.serve().await;
    });
    server_tx.send(client).await.unwrap();

    let mut counter = client_rx.recv().await.unwrap().unwrap();
    assert_eq!(counter.value().await.unwrap(), 0);

    // The server cannot decode this request, since it does not know the method.
    counter.added_later().await.unwrap_err();

    // Which must not have kept it from serving the methods it does know.
    assert_eq!(counter.increase(42).await.unwrap(), 42, "serving ended after an unknown method");
    assert_eq!(counter.value().await.unwrap(), 42);

    // Also when it happens again.
    counter.added_later().await.unwrap_err();
    assert_eq!(counter.value().await.unwrap(), 42, "serving ended after an unknown method");

    assert_eq!(obj.read().await.value, 42, "the calls did not reach the served object");
}
