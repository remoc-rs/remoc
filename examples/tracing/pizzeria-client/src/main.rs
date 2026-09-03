//! This crate implements the client of the remote pizzeria service.
#![warn(missing_docs)]

use remoc::prelude::*;
use std::{net::Ipv4Addr, time::Duration};
use tokio::net::TcpStream;
use tracing::{Span, instrument, level_filters::LevelFilter};

use pizzeria::{Pizzeria, PizzeriaClient, Progress, TCP_PORT, init_tracing};

/// Orders one of each pizza on the menu.
///
/// The `#[instrument]` attribute of the tracing crate creates a span for this
/// function, so that the spans of the calls executed by the server are
/// linked to it in one distributed trace.
#[instrument(skip(client))]
async fn order_pizzas(client: &PizzeriaClient) {
    // Query the menu. No span is created for this call, since the menu
    // method sets the tracing level "off".
    let menu = client.menu().await.unwrap();
    println!("On the menu today: {menu:?}\n");

    // Create the callback through which the server reports the progress of
    // the orders. It is executed here, at the client, whenever the server
    // calls it.
    //
    // Calls of a remote function are not traced by default. Enabling them at
    // info level makes the server create a span for each call and the client
    // one for processing it, which is linked into the trace of the order.
    // The name is recorded in these spans.
    //
    // The callback is only called while the orders are running, so its calls
    // are processed within the span of this function. The span then stays
    // open until the callback is dropped by the server and the client, which
    // happens shortly after the orders complete. To close it exactly when
    // this function returns, create the callback using `provided_1` and keep
    // its provider in this function instead.
    let mut progress = Progress::new_1(|step: String| async move {
        tracing::info!("progress report: {step}");
    });
    progress.set_name("progress");
    progress.set_tracing_level(LevelFilter::INFO);
    progress.set_span(Span::current());

    // Order one of each pizza, all at once.
    // The server prepares the pizzas in parallel, which is visible in
    // the timing of their spans within the trace.
    println!("Ordering one of each...");
    let deliveries =
        futures::future::try_join_all(menu.iter().map(|&pizza| client.order(pizza, progress.clone())))
            .await
            .unwrap();

    for delivery in deliveries {
        println!("Received: {delivery}");
    }
}

#[tokio::main]
async fn main() {
    // Initialize logging and trace export.
    let provider = init_tracing("pizzeria-client");

    // Establish TCP connection to server.
    let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, TCP_PORT)).await.unwrap();
    socket.set_nodelay(true).unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish a Remoc connection with default configuration over the TCP
    // connection and consume (i.e. receive) the pizzeria client from the server.
    let client: PizzeriaClient =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).consume().await.unwrap();

    order_pizzas(&client).await;

    // Send the spans of the order to the collector, after giving the span
    // of the order a moment to close, see above.
    if let Some(provider) = &provider {
        tokio::time::sleep(Duration::from_millis(100)).await;
        provider.force_flush().unwrap();
    }
}
