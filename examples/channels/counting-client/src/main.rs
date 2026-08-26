//! This crate implements the client of the counting example.
//!
//! It asks the server to count, sending the channel to count into along with
//! the request.
#![warn(missing_docs)]

use remoc::prelude::*;
use std::net::Ipv4Addr;
use tokio::net::TcpStream;

use counting::{CountReq, TCP_PORT};

#[tokio::main]
async fn main() {
    // Establish TCP connection to server.
    let socket = TcpStream::connect((Ipv4Addr::LOCALHOST, TCP_PORT)).await.unwrap();
    let (socket_rx, socket_tx) = socket.into_split();

    // Establish a Remoc connection with default configuration over the TCP
    // connection and obtain the sending half of the base channel.
    //
    // The connection is always bidirectional, but this end only sends, so the
    // unneeded receiver is dropped.
    let (conn, tx, _rx): (_, _, rch::base::Receiver<()>) =
        remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx).await.unwrap();

    // The connection dispatcher must be spawned; it drives everything that
    // travels over this TCP connection.
    tokio::spawn(conn);

    count(tx, 10).await;
}

/// Asks the server to count up to `up_to` and prints each number as it arrives.
async fn count(mut tx: rch::base::Sender<CountReq>, up_to: u32) {
    // Create a new channel. Nothing has been sent yet, so the server does not
    // know about it and no connection has been made for it.
    let (seq_tx, mut seq_rx) = rch::mpsc::channel();

    // Sending the sender half connects the channel to the server, inside the
    // TCP connection that already exists. There is no port to open, nothing to
    // register and no acknowledgement to wait for.
    println!("Asking the server to count up to {up_to}");
    tx.send(CountReq { up_to, seq_tx }).await.unwrap();

    // Receive each number as the server counts it.
    while let Some(i) = seq_rx.recv().await.unwrap() {
        println!("Server counts {i}");
    }

    // recv() returned None, so the server dropped its sender: the sequence is
    // over. A channel closing is how a remote endpoint says it is finished.
    println!("Server is done counting.");
}
