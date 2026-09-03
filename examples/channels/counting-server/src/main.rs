//! This crate implements the server of the counting example.
//!
//! It accepts TCP connections, establishes a Remoc connection over each one and
//! then counts into whatever channel the client sends it.
#![warn(missing_docs)]

use remoc::prelude::*;
use std::{net::Ipv4Addr, time::Duration};
use tokio::{net::TcpListener, time::sleep};

use counting::{CountReq, TCP_PORT};

#[tokio::main]
async fn main() {
    // Listen to TCP connections using Tokio.
    // In reality you would probably use TLS or WebSockets over HTTPS.
    println!("Listening on port {TCP_PORT}. Press Ctrl+C to exit.");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, TCP_PORT)).await.unwrap();

    loop {
        // Accept an incoming TCP connection.
        let (socket, addr) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        let (socket_rx, socket_tx) = socket.into_split();
        println!("Accepted connection from {addr}");

        // Spawn a task for each incoming connection.
        tokio::spawn(async move {
            // Establish a Remoc connection with default configuration over the TCP
            // connection and obtain the receiving half of the base channel.
            //
            // The connection is always bidirectional, but this end only receives,
            // so the unneeded sender is dropped.
            let (conn, _tx, rx): (_, rch::base::Sender<()>, _) =
                remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await.unwrap();

            // The connection dispatcher must be spawned; it drives everything that
            // travels over this TCP connection.
            tokio::spawn(conn);

            // Serve requests until the client disconnects. A client going away is
            // an ordinary end to the conversation, not a failure, so it is told
            // apart from the errors that are.
            match serve(rx).await {
                Ok(()) => println!("Client {addr} closed the connection"),
                Err(err) if err.is_disconnected() => println!("Client {addr} disconnected"),
                Err(err) => println!("Connection from {addr} failed: {err}"),
            }
        });
    }
}

/// Counts for the client, once per request.
async fn serve(mut rx: rch::base::Receiver<CountReq>) -> Result<(), rch::base::RecvError> {
    // Receive count requests over the base channel.
    while let Some(CountReq { up_to, seq_tx }) = rx.recv().await? {
        println!("Counting up to {up_to}");

        for i in 0..up_to {
            // Send each counted number over the channel the client provided.
            //
            // This is an ordinary channel send; that the receiver is on another
            // machine makes no difference to the code, only to the error it can
            // return.
            if seq_tx.send(i).await.into_disconnected().unwrap() {
                // The client dropped its receiver or went away, so there is
                // nobody left to count for.
                println!("Client stopped listening");
                break;
            }

            sleep(Duration::from_millis(300)).await;
        }

        // Dropping seq_tx closes that channel, which is how the client's
        // recv() learns that the sequence has ended.
    }

    Ok(())
}
