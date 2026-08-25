//! Behavior of remote calls when the connection is lost.

use std::{sync::Arc, time::Duration};
use wokio::time::{sleep, timeout};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::droppable_loop_channel;
use remoc::rtc::{CallError, ServerShared};

#[remoc::rtc::remote]
pub trait Slow {
    /// Returns only after the connection is long gone.
    async fn slow(&self) -> Result<u32, CallError>;

    /// Returns immediately.
    async fn quick(&self) -> Result<u32, CallError>;
}

pub struct SlowObj;

impl Slow for SlowObj {
    async fn slow(&self) -> Result<u32, CallError> {
        sleep(Duration::from_secs(3600)).await;
        Ok(1)
    }

    async fn quick(&self) -> Result<u32, CallError> {
        Ok(2)
    }
}

/// A call that is in flight when the connection fails must report the failure
/// instead of waiting for a reply that can never arrive.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn call_in_flight_fails_when_connection_is_lost() {
    crate::init();
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx), conn) = droppable_loop_channel::<SlowClient>().await;

    let (server, client) = SlowServerShared::new(Arc::new(SlowObj));
    a_tx.send(client).await.unwrap();
    wokio::spawn(async move { server.serve().await });

    let client = b_rx.recv().await.unwrap().unwrap();
    assert_eq!(client.quick().await.unwrap(), 2, "connection is not working");

    println!("Calling a method that never returns");
    let call = wokio::spawn(async move { client.slow().await });
    sleep(Duration::from_millis(100)).await;

    println!("Losing the connection");
    drop(conn);

    let res = timeout(Duration::from_secs(10), call).await;
    match res {
        Ok(Ok(Err(err))) => {
            println!("Call failed with: {err}");
            assert!(
                !matches!(err, CallError::NotServed),
                "a lost connection must not be reported as the object not being served"
            );
        }
        Ok(Ok(Ok(value))) => panic!("call succeeded with {value} although the connection was lost"),
        Ok(Err(err)) => panic!("call task failed: {err}"),
        Err(_) => panic!("call did not return after the connection was lost"),
    }
}

/// A call issued after the connection failed must report the failure rather than
/// waiting for a reply.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn call_after_connection_loss_fails() {
    crate::init();
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx), conn) = droppable_loop_channel::<SlowClient>().await;

    let (server, client) = SlowServerShared::new(Arc::new(SlowObj));
    a_tx.send(client).await.unwrap();
    wokio::spawn(async move { server.serve().await });

    let client = b_rx.recv().await.unwrap().unwrap();
    assert_eq!(client.quick().await.unwrap(), 2, "connection is not working");

    println!("Losing the connection");
    drop(conn);
    sleep(Duration::from_millis(100)).await;

    match timeout(Duration::from_secs(10), client.quick()).await {
        Ok(Err(err)) => println!("Call failed with: {err}"),
        Ok(Ok(value)) => panic!("call succeeded with {value} although the connection was lost"),
        Err(_) => panic!("call did not return after the connection was lost"),
    }
}
