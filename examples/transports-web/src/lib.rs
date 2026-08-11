//! Compile-checks the web transport snippets embedded into the Remoc documentation.
//!
//! The snippets live inside `remoc/src/` so that `include_str!` keeps working in
//! the published crate; they are pulled in here by path.
//!
//! These snippets require a WebAssembly target with a JavaScript runtime
//! environment, so they cannot be part of `examples/transports`.

#[path = "../../../remoc/src/connect/transports/websocket_web.rs"]
pub mod websocket_web;
