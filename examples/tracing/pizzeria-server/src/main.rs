//! This crate implements the server of the remote pizzeria service.
#![warn(missing_docs)]

use remoc::{codec, prelude::*};
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, time::sleep};
use tracing::{Instrument, info_span, instrument};

use pizzeria::{Pizza, Pizzeria, PizzeriaServerShared, Progress, TCP_PORT, init_tracing};

/// Server object for the pizzeria service.
pub struct PizzeriaObj;

impl PizzeriaObj {
    /// Prepares the dough.
    #[instrument(skip(self))]
    async fn prepare_dough(&self, pizza: Pizza) {
        sleep(Duration::from_millis(300)).await;
        tracing::info!("the dough is ready");
    }

    /// Puts the toppings for the specified pizza onto the dough.
    #[instrument(skip(self))]
    async fn add_toppings(&self, pizza: Pizza) {
        sleep(Duration::from_millis(150)).await;
    }

    /// Bakes the pizza.
    #[instrument(skip(self))]
    async fn bake(&self) {
        sleep(Duration::from_millis(500)).await;
        tracing::info!("out of the oven");
    }
}

/// Implementation of the remote pizzeria service.
impl Pizzeria for PizzeriaObj {
    async fn menu(&self) -> Result<Vec<Pizza>, rtc::CallError> {
        Ok(vec![Pizza::Margherita, Pizza::Salami, Pizza::Hawaii])
    }

    async fn order(&self, pizza: Pizza, progress: Progress) -> Result<String, rtc::CallError> {
        // These methods carry the #[instrument] attribute of the tracing
        // crate, thus their spans become children of the span of this call.
        //
        // After each step the progress callback of the client is called.
        // This is a remote function call in the opposite direction, whose
        // spans are linked into the trace of this call as well.
        // Reporting is best-effort, so failures to reach the client are
        // ignored and the pizza is finished regardless.
        self.prepare_dough(pizza).await;
        let _ = progress.try_call(format!("{pizza:?}: dough is ready")).await;
        self.add_toppings(pizza).await;
        let _ = progress.try_call(format!("{pizza:?}: toppings are on")).await;
        self.bake().await;
        let _ = progress.try_call(format!("{pizza:?}: baked")).await;

        Ok(format!("{pizza:?} pizza, fresh out of the oven"))
    }
}

#[tokio::main]
async fn main() {
    // Initialize logging and trace export.
    let provider = init_tracing("pizzeria-server");

    // Create a pizzeria object that will be shared between all clients.
    let pizzeria_obj = Arc::new(PizzeriaObj);

    let serve = async move {
        // Listen to TCP connections using Tokio.
        // In reality you would probably use TLS or WebSockets over HTTPS.
        println!("Listening on port {}. Press Ctrl+C to exit.", TCP_PORT);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, TCP_PORT)).await.unwrap();

        loop {
            // Accept an incoming TCP connection.
            let (socket, addr) = listener.accept().await.unwrap();
            socket.set_nodelay(true).unwrap();
            let (socket_rx, socket_tx) = socket.into_split();
            println!("Accepted connection from {}", addr);

            // Create a new shared reference to the pizzeria object.
            let pizzeria_obj = pizzeria_obj.clone();

            // Spawn a task for each incoming connection.
            tokio::spawn(
                async move {
                    // Create a server proxy and client for the accepted connection.
                    //
                    // The server proxy executes all incoming method calls on the
                    // shared pizzeria_obj.
                    //
                    // Current limitations of the Rust compiler require that we
                    // explicitly specify the codec.
                    let (server, client) = PizzeriaServerShared::<_, codec::Default>::new(pizzeria_obj);

                    // Establish a Remoc connection with default configuration over
                    // the TCP connection and provide (i.e. send) the pizzeria
                    // client to the client.
                    remoc::Connect::io(remoc::Cfg::default(), socket_rx, socket_tx)
                        .provide(client)
                        .await
                        .unwrap();

                    // Serve incoming requests from the client on this task.
                    // Requests are executed in parallel, so several pizzas can be
                    // prepared at once.
                    server.serve().await.unwrap();
                }
                .instrument(info_span!("incoming", %addr)),
            );
        }
    };

    // Serve until Ctrl+C is pressed.
    tokio::select! {
        () = serve => (),
        _ = tokio::signal::ctrl_c() => (),
    }

    // Send the remaining spans to the collector.
    if let Some(provider) = provider {
        provider.shutdown().unwrap();
    }
}
