//! This crate implements the client of the remote counting service.
#![warn(missing_docs)]

use remoc::prelude::*;
use std::{net::Ipv4Addr, time::Duration};
use tokio::net::TcpStream;

use counter::{Counter, CounterClient, TCP_PORT};

#[tokio::main]
async fn main() {
    // Initialize logging to the terminal at info level, overridable using
    // the `RUST_LOG` environment variable.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // Establish TCP connection to server.
    let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, TCP_PORT)).await.unwrap();
    socket.set_nodelay(true).unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish a Remoc connection with default configuration over the TCP connection and
    // consume (i.e. receive) the counter client from the server.
    let mut client: CounterClient =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).consume().await.unwrap();

    // Subscribe to the counter watch and print each value change.
    println!("Subscribing to counter change notifications");
    let mut watch_rx = client.watch().await.unwrap();
    let watch_task = tokio::spawn(async move {
        while watch_rx.changed().await.is_ok() {
            let value = watch_rx.borrow_and_update().unwrap();
            println!("Counter change notification: {}", *value);
        }
    });
    println!("Done!");

    // Print current value.
    let value = client.value().await.unwrap();
    println!("Current counter value is {}\n", value);

    // Increase counter value.
    println!("Increasing counter value by 5");
    client.increase(5).await.unwrap();
    println!("Done!\n");

    // Print new value.
    let value = client.value().await.unwrap();
    println!("New counter value is {}\n", value);

    // Let the server count to the current value.
    println!("Asking the server to count to the current value with a step delay of 300ms...");
    let mut rx = client.count_to_value(1, Duration::from_millis(300)).await.unwrap();
    while let Ok(Some(i)) = rx.recv().await {
        println!("Server counts {}", i);
    }
    println!("Server is done counting.\n");

    // Wait for watch task.
    println!("Server exercise is done.");
    println!("You can now press Ctrl+C to exit or continue watching for value ");
    println!("change notification caused by other clients.");
    watch_task.await.unwrap();
}
