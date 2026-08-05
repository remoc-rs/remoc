//! Compile-checks the transport snippets embedded into the Remoc documentation.
//!
//! The snippets live inside `remoc/src/` so that `include_str!` keeps working in
//! the published crate; they are pulled in here by path.

#[path = "../../../remoc/src/connect/transports/aggligator.rs"]
pub mod aggligator;

#[path = "../../../remoc/src/connect/transports/process.rs"]
pub mod process;

#[path = "../../../remoc/src/connect/transports/tcp.rs"]
pub mod tcp;

#[path = "../../../remoc/src/connect/transports/tls_client.rs"]
pub mod tls_client;

#[path = "../../../remoc/src/connect/transports/tls_server.rs"]
pub mod tls_server;

#[path = "../../../remoc/src/connect/transports/websocket_axum.rs"]
pub mod websocket_axum;

#[path = "../../../remoc/src/connect/transports/websocket_client.rs"]
pub mod websocket_client;
