//! Browser client for the shared RTC counter.

use bytes::Bytes;
use counter_web::{Counter, CounterClient};
use futures::{SinkExt, StreamExt, future, lock::Mutex};
use js_sys::Function;
use remoc::prelude::*;
use std::{fmt, io, rc::Rc};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use websocket_web::{Msg, WebSocket};

/// A browser-facing handle to the remote counter.
// wasm-bindgen makes this type and its public methods available to JavaScript.
#[wasm_bindgen]
pub struct WebCounter {
    client: Rc<Mutex<CounterClient>>,
}

#[wasm_bindgen]
impl WebCounter {
    /// Connects to the counter and starts forwarding watched values to JavaScript.
    #[wasm_bindgen(js_name = connect)]
    pub async fn connect(
        url: String, on_value: Function, on_disconnect: Function,
    ) -> Result<WebCounter, JsValue> {
        let websocket = WebSocket::connect(&url).await.map_err(js_error)?;
        let (websocket_tx, websocket_rx) = websocket.into_split();

        // Adapt binary WebSocket messages to the packet sink and stream Remoc expects.
        let transport_tx =
            websocket_tx.with(|packet: Bytes| future::ready(Ok::<_, io::Error>(Msg::Binary(packet.into()))));
        let transport_rx = websocket_rx.filter_map(|message| {
            future::ready(match message {
                Ok(Msg::Binary(packet)) => Some(Ok(Bytes::from(packet))),
                Ok(Msg::Text(_)) => None,
                Err(error) => Some(Err(error)),
            })
        });

        // Start Remoc on the WebSocket and receive the CounterClient sent by the server.
        let client: CounterClient = remoc::Connect::framed(remoc::Cfg::default(), transport_tx, transport_rx)
            .consume()
            .await
            .map_err(js_error)?;

        // Send the current value to JavaScript before waiting for later changes.
        let mut watch = client.watch().await.map_err(js_error)?;
        let initial_value = *watch.borrow_and_update().map_err(js_error)?;
        call_with_value(&on_value, initial_value)?;

        // Browser futures run on the current JavaScript event-loop thread.
        spawn_local(async move {
            if let Err(error) = forward_values(watch, on_value).await {
                report_disconnect(&on_disconnect, error);
            }
        });

        Ok(Self { client: Rc::new(Mutex::new(client)) })
    }

    /// Increases the shared counter by one.
    pub async fn increment(&self) -> Result<(), JsValue> {
        self.client.lock().await.increment().await.map_err(js_error)
    }

    /// Decreases the shared counter by one.
    pub async fn decrement(&self) -> Result<(), JsValue> {
        self.client.lock().await.decrement().await.map_err(js_error)
    }
}

/// Forwards watched counter values to JavaScript until the channel closes.
async fn forward_values(
    mut watch: rch::watch::Receiver<u32>, on_value: Function,
) -> Result<(), rch::watch::ChangedError> {
    loop {
        watch.changed().await?;
        let value = *watch.borrow_and_update()?;
        if let Err(error) = call_with_value(&on_value, value) {
            web_sys::console::error_2(&"The value callback failed:".into(), &error);
        }
    }
}

fn call_with_value(callback: &Function, value: u32) -> Result<(), JsValue> {
    callback.call1(&JsValue::NULL, &JsValue::from(value))?;
    Ok(())
}

fn report_disconnect(error_callback: &Function, error: impl fmt::Display) {
    if let Err(callback_error) = error_callback.call1(&JsValue::NULL, &JsValue::from_str(&error.to_string())) {
        web_sys::console::error_2(&"The disconnection callback failed:".into(), &callback_error);
    }
}

fn js_error(error: impl fmt::Display) -> JsValue {
    js_sys::Error::new(&error.to_string()).into()
}
