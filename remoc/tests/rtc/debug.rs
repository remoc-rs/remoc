//! The `debug` argument of the `remote` attribute.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::{Arc, Mutex};

use futures::{FutureExt, future::BoxFuture};
use remoc::{
    prelude::*,
    rtc::{
        CallError, Req, ReqEnum, ServerShared,
        monitor::{CallDecision, ClientMonitor, MonitorableClient},
    },
};

use crate::loop_channel;

#[rtc::remote(debug)]
pub trait Greeter {
    async fn greet(&self, name: String, times: u32) -> Result<String, CallError>;
}

pub struct GreeterObj;

impl Greeter for GreeterObj {
    async fn greet(&self, name: String, times: u32) -> Result<String, CallError> {
        Ok(name.repeat(times as usize))
    }
}

/// Records the `Debug` representation of every request it sees.
#[derive(Clone, Default)]
struct RecordingMonitor(Arc<Mutex<Vec<String>>>);

impl<Value, Ref, RefMut> ClientMonitor<Value, Ref, RefMut> for RecordingMonitor
where
    Value: ReqEnum + std::fmt::Debug,
    Ref: ReqEnum + std::fmt::Debug,
    RefMut: ReqEnum + std::fmt::Debug,
{
    fn pre_call<'a>(&'a self, req: &'a Req<Value, Ref, RefMut>) -> BoxFuture<'a, CallDecision> {
        self.0.lock().unwrap().push(format!("{req:?}"));
        async { CallDecision::Pass }.boxed()
    }
}

/// The request enums implement `Debug`, showing the method and its arguments.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn requests_are_debug() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<GreeterClient>().await;

    let (server, client) = GreeterServerShared::new(Arc::new(GreeterObj));
    a_tx.send(client).await.unwrap();

    let recorded = RecordingMonitor::default();
    let recorded_in_task = recorded.clone();

    let client_task = async move {
        let mut client = b_rx.recv().await.unwrap().unwrap();
        client.set_monitor(recorded_in_task);

        assert_eq!(client.greet("ab".to_string(), 2).await.unwrap(), "abab");
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();

    let recorded = recorded.0.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "the request was not recorded: {recorded:?}");
    let req = &recorded[0];
    assert!(req.contains("Greet"), "the method is missing from {req}");
    assert!(req.contains("ab"), "the arguments are missing from {req}");
    assert!(req.contains('2'), "the arguments are missing from {req}");
}
