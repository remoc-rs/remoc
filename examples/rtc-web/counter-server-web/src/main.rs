//! Axum server for the shared web counter.

use anyhow::Context;
use axum::{
    Router,
    body::Bytes,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Response},
    routing::{any, get},
    serve::ListenerExt,
};
use counter_web::{ChangeError, Counter, CounterServerSharedMut, HTTP_PORT};
use futures::{SinkExt, StreamExt, future};
use remoc::{codec, prelude::*};
use std::{net::Ipv4Addr, sync::Arc};
use tokio::sync::RwLock;

// These files become part of the server executable at compile time.
const INDEX_HTML: &str = include_str!("index.html");
const CLIENT_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/web/counter_web.js"));
const CLIENT_WASM: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/web/counter_web_bg.wasm"));

type SharedCounter = Arc<RwLock<CounterObj>>;

/// The shared counter state.
struct CounterObj {
    value: rch::watch::Sender<u32>,
}

impl Default for CounterObj {
    fn default() -> Self {
        let (value, _) = rch::watch::channel(0);
        Self { value }
    }
}

impl Counter for CounterObj {
    async fn increment(&mut self) -> Result<(), ChangeError> {
        let current = *self.value.borrow();
        let value = current.checked_add(1).ok_or(ChangeError::Maximum)?;
        self.value.send_replace(value);
        Ok(())
    }

    async fn decrement(&mut self) -> Result<(), ChangeError> {
        let current = *self.value.borrow();
        let value = current.checked_sub(1).ok_or(ChangeError::Minimum)?;
        self.value.send_replace(value);
        Ok(())
    }

    async fn watch(&self) -> Result<rch::watch::Receiver<u32>, rtc::CallError> {
        Ok(self.value.subscribe())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging to the terminal at info level, overridable using
    // the `RUST_LOG` environment variable.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let counter = Arc::new(RwLock::new(CounterObj::default()));
    let app = Router::new()
        .route("/", get(index))
        .route("/counter_web.js", get(client_js))
        .route("/counter_web_bg.wasm", get(client_wasm))
        .route("/remoc", any(websocket))
        .with_state(counter);

    let address = (Ipv4Addr::LOCALHOST, HTTP_PORT);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to listen on {}:{}", address.0, address.1))?
        .tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });

    println!("Open http://{}:{} in a web browser.", address.0, address.1);
    axum::serve(listener, app).await.context("web server failed")
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn client_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8"))], CLIENT_JS)
}

async fn client_wasm() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, HeaderValue::from_static("application/wasm"))], Bytes::from_static(CLIENT_WASM))
}

async fn websocket(State(counter): State<SharedCounter>, upgrade: WebSocketUpgrade) -> Response {
    // Axum hands the upgraded WebSocket to this future.
    upgrade.on_upgrade(move |socket| async move {
        if let Err(error) = serve_client(socket, counter).await {
            tracing::warn!(%error, "remoc client connection failed");
        }
    })
}

async fn serve_client(socket: WebSocket, counter: SharedCounter) -> anyhow::Result<()> {
    let (websocket_tx, websocket_rx) = socket.split();

    // Adapt Axum's binary WebSocket messages to the packet sink and stream Remoc expects.
    let transport_tx =
        websocket_tx.with(|packet: Bytes| future::ready(Ok::<_, axum::Error>(Message::Binary(packet))));
    let transport_rx = websocket_rx.filter_map(|message| {
        future::ready(match message {
            Ok(Message::Binary(packet)) => Some(Ok(packet)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
    });

    // The macro-generated server executes calls on the counter shared by all connections.
    let (server, client) = CounterServerSharedMut::<_, codec::Default>::new(counter);

    // Send its client proxy to the browser, then serve calls arriving through that proxy.
    remoc::Connect::framed(remoc::Cfg::default(), transport_tx, transport_rx)
        .provide(client)
        .await
        .context("failed to establish Remoc connection")?;
    server.serve().await.map_err(|error| anyhow::anyhow!("failed to serve counter: {error}"))
}
