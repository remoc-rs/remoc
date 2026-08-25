//! The `sequential` and `stop_on_error` call options of a client.

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

/// Runs four `work` calls at once and returns how many of them the server ran
/// concurrently.
///
/// `parallelism` is applied to the server when given, otherwise it keeps its default.
async fn max_concurrent_with(sequential: bool, parallelism: Option<usize>) -> usize {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    let (mut server, client) = WorkerServerShared::new(Arc::new(WorkerObj::default()));
    if let Some(parallelism) = parallelism {
        server.set_parallelism(parallelism);
        assert_eq!(server.parallelism(), parallelism);
    } else {
        assert_eq!(server.parallelism(), rtc::DEFAULT_PARALLELISM);
    }
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_sequential(sequential);
        assert_eq!(client.sequential(), sequential);

        // Start all calls before awaiting any of them, so that they reach the
        // server while the preceding ones are still running.
        let calls = [
            client.work_call().await,
            client.work_call().await,
            client.work_call().await,
            client.work_call().await,
        ];
        for call in calls {
            call.await.unwrap();
        }

        client.max_concurrent().await.unwrap()
    };

    let (max_concurrent, res) = tokio::join!(client_task, server.serve());
    res.unwrap();
    max_concurrent
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn concurrent_dispatch_is_default() {
    assert_eq!(max_concurrent_with(false, None).await, 4);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sequential_client_is_dispatched_inline() {
    assert_eq!(max_concurrent_with(true, None).await, 1);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn stop_on_error_stops_the_server() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<WorkerClient>().await;

    let (server, client) = WorkerServerShared::new(Arc::new(WorkerObj::default()));
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_stop_on_error(true);
        assert!(client.stop_on_error());

        // The response is queued before the server is stopped, thus the error of the
        // call itself is received.
        assert!(matches!(client.fail().await, Err(WorkError::Failed)));
    };

    let ((), res) = tokio::join!(client_task, server.serve());
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

    let (server, client) = WorkerServerShared::new(Arc::new(WorkerObj::default()));
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();
        assert!(!client.stop_on_error());

        assert!(matches!(client.fail().await, Err(WorkError::Failed)));

        // The server keeps serving after the failed call.
        client.work().await.unwrap();
        assert_eq!(client.max_concurrent().await.unwrap(), 1);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}

/// The server dispatches at most `parallelism` calls at the same time.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn parallelism_limits_concurrent_calls() {
    assert_eq!(max_concurrent_with(false, Some(2)).await, 2);
}

/// A parallelism of one runs a single call at a time, but on its own task.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn parallelism_of_one_serves_sequentially() {
    assert_eq!(max_concurrent_with(false, Some(1)).await, 1);
}

/// A parallelism of zero dispatches calls inline.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn parallelism_of_zero_dispatches_inline() {
    assert_eq!(max_concurrent_with(false, Some(0)).await, 1);
}
