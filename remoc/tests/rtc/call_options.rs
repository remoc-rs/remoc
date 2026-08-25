//! The `allow_spawn` and `stop_on_error` call options of a client.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};

use remoc::{
    prelude::*,
    rtc::{CallError, Client, ServeError, ServerShared},
};

use crate::loop_channel;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum WorkError {
    Failed,
    Call(CallError),
}

impl From<CallError> for WorkError {
    fn from(err: CallError) -> Self {
        Self::Call(err)
    }
}

#[rtc::remote]
pub trait Worker {
    /// Records how many calls of this method run at the same time.
    async fn work(&self) -> Result<(), WorkError>;

    /// The highest number of [`work`](Self::work) calls seen at the same time.
    async fn max_concurrent(&self) -> Result<usize, WorkError>;

    /// Always fails.
    async fn fail(&self) -> Result<(), WorkError>;
}

#[derive(Default)]
pub struct WorkerObj {
    in_flight: AtomicUsize,
    max_concurrent: AtomicUsize,
}

impl Worker for WorkerObj {
    async fn work(&self) -> Result<(), WorkError> {
        let in_flight = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.max_concurrent.fetch_max(in_flight, SeqCst);
        wokio::time::sleep(Duration::from_millis(50)).await;
        self.in_flight.fetch_sub(1, SeqCst);
        Ok(())
    }

    async fn max_concurrent(&self) -> Result<usize, WorkError> {
        Ok(self.max_concurrent.load(SeqCst))
    }

    async fn fail(&self) -> Result<(), WorkError> {
        Err(WorkError::Failed)
    }
}

/// Runs three `work` calls at once against a server started with `spawn` enabled
/// and returns how many of them the server ran concurrently.
async fn max_concurrent_with(allow_spawn: bool) -> usize {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    let (server, client) = WorkerServerShared::new(Arc::new(WorkerObj::default()), 16);
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_allow_spawn(allow_spawn);
        assert_eq!(client.allow_spawn(), allow_spawn);

        // Start all calls before awaiting any of them, so that they reach the
        // server while the preceding ones are still running.
        let calls = [client.work_call().await, client.work_call().await, client.work_call().await];
        for call in calls {
            call.await.unwrap();
        }

        client.max_concurrent().await.unwrap()
    };

    let (max_concurrent, res) = tokio::join!(client_task, server.serve(true));
    res.unwrap();
    max_concurrent
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn allow_spawn_is_default() {
    assert_eq!(max_concurrent_with(true).await, 3);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn allow_spawn_disabled_serves_sequentially() {
    assert_eq!(max_concurrent_with(false).await, 1);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn stop_on_error_stops_the_server() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    let (server, client) = WorkerServerShared::new(Arc::new(WorkerObj::default()), 16);
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_stop_on_error(true);
        assert!(client.stop_on_error());

        // The reply is queued before the server is stopped, thus the error of the
        // call itself is received.
        assert!(matches!(client.fail().await, Err(WorkError::Failed)));
    };

    let ((), res) = tokio::join!(client_task, server.serve(true));
    assert!(
        matches!(res, Err(ServeError::CallFailed { method: "Worker::fail" })),
        "server did not stop on error: {res:?}"
    );
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn stop_on_error_is_off_by_default() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    let (server, client) = WorkerServerShared::new(Arc::new(WorkerObj::default()), 16);
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();
        assert!(!client.stop_on_error());

        assert!(matches!(client.fail().await, Err(WorkError::Failed)));

        // The server keeps serving after the failed call.
        client.work().await.unwrap();
        assert_eq!(client.max_concurrent().await.unwrap(), 1);
    };

    let ((), res) = tokio::join!(client_task, server.serve(true));
    res.unwrap();
}

