# Distributed tracing over RTC

This example implements a pizzeria over RTC and links the tracing spans of the
client and the server into one distributed trace.

The pizzeria trait specifies the `tracing` argument in its `remote` attribute,
so the client creates a span for each call and the server one for processing
it, which is linked into the trace of the client. The client orders several
pizzas concurrently within one span. The server prepares each pizza in steps
that carry the `#[instrument]` attribute of the tracing crate, showing how
remotely and locally created spans nest within one trace.

The client also passes a remote function to each order, through which the
server reports the progress of the preparation. Calling it is a remote call
in the opposite direction, from the server to the client, and its spans are
linked into the same trace, since the client enables tracing on the function.

It is split into three crates:

  * `pizzeria` provides the remote trait definition, the type of the progress
    callback and the tracing setup shared between client and server.
  * `pizzeria-server` implements the pizzeria server and accepts connections
    over TCP.
  * `pizzeria-client` implements a client that orders one of each pizza on
    the menu.

If you are new to RTC, start with [the RTC example](../rtc), which this
example builds upon.

## Running without a collector

Start the server using the following command:

    cargo run --manifest-path examples/tracing/Cargo.toml -p pizzeria-server

And, in another terminal, start the client using the following command:

    cargo run --manifest-path examples/tracing/Cargo.toml -p pizzeria-client

Both log to the terminal using the fmt layer of tracing-subscriber, with
opening and closing spans logged as well. Without OpenTelemetry, Remoc identifies each
call by a random span id, which is recorded in the `span_id` field
of the span of the call at the client:

    INFO order_pizzas:call{otel.name="Pizzeria::order" otel.kind="client" span_id=0253f0dff882434b}: remoc::rtc::call: close time.busy=117µs time.idle=998ms

The server records it in the same field of the span processing the call,
so the log lines of both sides can be matched by searching for the id:

    INFO incoming{addr=127.0.0.1:49126}:call{otel.name="Pizzeria::order" otel.kind="server" span_id=0253f0dff882434b}:prepare_dough{pizza=Margherita}: pizzeria_server: the dough is ready
    INFO incoming{addr=127.0.0.1:49126}:call{otel.name="Pizzeria::order" otel.kind="server" span_id=0253f0dff882434b}:bake: pizzeria_server: out of the oven
    INFO incoming{addr=127.0.0.1:49126}:call{otel.name="Pizzeria::order" otel.kind="server" span_id=0253f0dff882434b}: remoc::rtc::call: close time.busy=494µs time.idle=954ms

The calls of the progress callback work the same way with the roles swapped.
The server creates the span of the call, nested within the span processing
the order, and uses the target `remoc::rfn::call`:

    INFO incoming{addr=127.0.0.1:49126}:call{otel.name="Pizzeria::order" otel.kind="server" span_id=0253f0dff882434b}:call{otel.name="progress" otel.kind="client" span_id=53a42815129e4234}: remoc::rfn::call: close time.busy=74.0µs time.idle=42.1ms

The client processes the call within a span carrying the same id:

    INFO order_pizzas:call{otel.name="progress" otel.kind="server" span_id=53a42815129e4234}: pizzeria_client: Margherita: dough is ready

## Running with a collector

Client and server export their spans via OTLP when the environment variable
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. To view the traces, run a collector,
for example [otel-tui], which displays them directly in the terminal, in a
separate terminal:

    otel-tui

Then start the server using the following command:

    OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --manifest-path examples/tracing/Cargo.toml -p pizzeria-server

And, in another terminal, start the client using the following command:

    OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 cargo run --manifest-path examples/tracing/Cargo.toml -p pizzeria-client

The collector shows the trace of the order, with the three orders
executing in parallel on the server and the progress reports crossing back
to the client:

    [pizzeria-client] order_pizzas
    ├─ [pizzeria-client] Pizzeria::order
    │  └─ [pizzeria-server] Pizzeria::order
    │     ├─ prepare_dough
    │     ├─ [pizzeria-server] progress
    │     │  └─ [pizzeria-client] progress
    │     ├─ add_toppings
    │     ├─ [pizzeria-server] progress
    │     │  └─ [pizzeria-client] progress
    │     ├─ bake
    │     └─ [pizzeria-server] progress
    │        └─ [pizzeria-client] progress
    ├─ [pizzeria-client] Pizzeria::order
    │  └─ ...
    └─ [pizzeria-client] Pizzeria::order
       └─ ...

Any other OTLP collector, for example [Jaeger], works as well:

    docker run --rm -p 16686:16686 -p 4317:4317 jaegertracing/jaeger

The traces are then browsable at `http://localhost:16686`.

All commands assume that you are in the top-level repository directory.

[otel-tui]: https://github.com/ymtdzzz/otel-tui
[Jaeger]: https://www.jaegertracing.io
