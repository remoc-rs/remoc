mod assoc_bounds;
mod assoc_collision;
mod assoc_mixed;
mod assoc_multiple;
mod assoc_qualified;
mod assoc_simple;
mod async_trait;
mod call_options;
mod debug;
mod default;
mod disconnect;
mod errors;
mod generics;
mod generics_non_clone;
mod monitor;
mod monitor_log;
mod no_server;
mod pipelined;
mod pipelined_chain;
mod pipelining;
mod readonly;
mod rename;
mod req_receiver_forward;
mod req_receiver_server;
mod serde_with;
mod simple;
mod simple_clone;
mod simple_req;
mod simple_req_remote;
mod simple_req_stream;
mod simple_rpit;
mod tracing_level;
mod value;
mod variants;
mod version_skew;

// Measures round trips on a paused clock, which requires a native Tokio runtime.
#[cfg(not(target_family = "wasm"))]
mod pipelined_round_trips;

// Must result in compile error:
// mod lifetime;
// mod reserved;
