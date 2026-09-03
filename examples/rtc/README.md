# Remote trait calling (RTC) counter example

This example implements a simple counter over RTC.
The server keeps a counter variable that can be remotely queried and increased
by a client.
Furthermore, it provides notifications to all subscribed clients when the
value is changed.

It is split into three crates:

  * `counter` provides the remote trait definition and error types shared
    between client and server.
  * `counter-server` implements the counter server and accepts connections 
    over TCP.
  * `counter-client` implements a simple counter client that exercises the server.

For the same idea built on plain channels instead of a trait, see
[the channels example](../channels).
For a Rust WebAssembly client connected through a browser WebSocket, see
[the web RTC example](../rtc-web).
For distributed tracing of the calls via OpenTelemetry, see
[the tracing example](../tracing).

## Running

Start the server using the following command:

    cargo run --manifest-path examples/rtc/Cargo.toml -p counter-server

Then, in another terminal, start the client using the following command:

    cargo run --manifest-path examples/rtc/Cargo.toml -p counter-client

All commands assume that you are in the top-level repository directory.
