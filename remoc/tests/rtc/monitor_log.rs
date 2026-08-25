//! The logging monitor.

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use std::sync::{Arc, Mutex};
use tracing::Level;

use remoc::{
    prelude::*,
    rtc::{CallError, ServerShared, monitor::LogMonitor},
};

use crate::loop_channel;

#[rtc::remote]
pub trait Greeter {
    /// Logged at the default level.
    async fn hello(&self) -> Result<u32, CallError>;

    /// Silenced by a per-method override.
    async fn chatty(&self) -> Result<u32, CallError>;

    /// Always fails, so it is logged at the failure level.
    async fn boom(&self) -> Result<u32, CallError>;
}

pub struct GreeterObj;

impl Greeter for GreeterObj {
    async fn hello(&self) -> Result<u32, CallError> {
        Ok(1)
    }

    async fn chatty(&self) -> Result<u32, CallError> {
        Ok(2)
    }

    async fn boom(&self) -> Result<u32, CallError> {
        Err(CallError::Dropped)
    }
}

/// Collects everything the subscriber writes.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Serves a greeter with the specified monitor installed and calls every method,
/// returning what was logged.
async fn served_with(monitor: LogMonitor) -> String {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(Level::TRACE)
        .without_time()
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<GreeterClient>().await;

    let (mut server, client) = GreeterServerShared::new(Arc::new(GreeterObj));
    server.set_monitor(monitor);
    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.hello().await.unwrap(), 1);
        assert_eq!(client.chatty().await.unwrap(), 2);
        assert!(client.boom().await.is_err());
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();

    captured.text()
}

/// Every request is logged, and its outcome with it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn logs_requests_and_outcomes() {
    let log = served_with(LogMonitor::new(Some(Level::INFO))).await;

    assert!(log.contains("Greeter::hello"), "the request was not logged:\n{log}");
    assert!(log.contains("dispatching"), "the start of the request was not logged:\n{log}");
    assert!(log.contains("done"), "the outcome of the request was not logged:\n{log}");
    assert!(log.contains("elapsed"), "the duration was not logged:\n{log}");
}

/// A per-method level of `None` silences that method and nothing else.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn per_method_level_silences_one_method() {
    let log = served_with(LogMonitor::new(Some(Level::INFO)).method("chatty", None)).await;

    assert!(log.contains("Greeter::hello"), "an unrelated method was silenced too:\n{log}");
    assert!(!log.contains("Greeter::chatty"), "the silenced method was logged anyway:\n{log}");
}

/// A failing call is logged at the failure level, whatever its method's level is.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn failure_is_logged_at_the_failure_level() {
    let log = served_with(LogMonitor::new(Some(Level::INFO)).failure_level(Some(Level::ERROR))).await;

    let failed = log.lines().find(|line| line.contains("failed")).unwrap_or_default();
    assert!(failed.contains("ERROR"), "the failure was not logged at the failure level:\n{log}");
    assert!(failed.contains("Greeter::boom"), "the failing method was not named:\n{log}");
}
