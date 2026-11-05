//! A foreground daemon that keeps timers alive independently of its clients.

use clap::Parser;
use remoc::{codec, prelude::*};
use std::{collections::BTreeMap, sync::Arc};
use timer_daemon_example::{AddError, NewTimer, Timer, TimerService, TimerServiceServerShared, local};
use tokio::{
    sync::Mutex,
    time::{Instant, sleep_until},
};

#[derive(Parser)]
#[command(about = "Run a local reminder daemon")]
struct Args {
    /// Unix socket path or Windows named-pipe name.
    #[arg(long, default_value = local::DEFAULT_ENDPOINT)]
    endpoint: String,
}

#[derive(Default)]
struct State {
    next_id: u64,
    pending: BTreeMap<u64, Timer>,
    events: rch::broadcast::Sender<Timer>,
}

#[derive(Default)]
struct Timers {
    // RPC handlers and independently running timer tasks share this state.
    state: Arc<Mutex<State>>,
}

impl TimerService for Timers {
    async fn add(&self, timer: NewTimer) -> Result<u64, AddError> {
        let NewTimer { after, message, notify } = timer;
        let deadline = Instant::now().checked_add(after).ok_or(AddError::InvalidDelay)?;
        let id = {
            let mut state = self.state.lock().await;
            let id = state.next_id.checked_add(1).ok_or(AddError::IdExhausted)?;
            state.next_id = id;
            state.pending.insert(id, Timer { id, message });
            id
        };

        // The RPC returns the id immediately. This task keeps running even if
        // the client disconnects after scheduling the timer.
        let state = self.state.clone();
        tokio::spawn(async move {
            sleep_until(deadline).await;
            let mut state = state.lock().await;
            let timer = state.pending.remove(&id).expect("scheduled timer exists");
            println!("Finished {}: {}", timer.id, timer.message);

            // This sender arrived inside NewTimer. Its receiver is still in the
            // waiting client, reached over the connection that carried add().
            if let Some(notify) = notify
                && let Err(err) = notify.send(timer.clone())
            {
                eprintln!("Timer notification failed: {err}");
            }

            // Also notify every watcher. Each receiver uses its own client's
            // existing connection; broadcasting needs no new connections.
            match state.events.send(timer) {
                Ok(_) => (),
                // Timers also run when nobody is watching.
                Err(rch::broadcast::SendError::Closed(_)) => (),
                Err(err) => eprintln!("Timer notification failed: {err}"),
            }
        });

        Ok(id)
    }

    async fn list(&self) -> Result<Vec<Timer>, rtc::CallError> {
        Ok(self.state.lock().await.pending.values().cloned().collect())
    }

    async fn subscribe(&self) -> Result<rch::broadcast::Receiver<Timer>, rtc::CallError> {
        Ok(self.state.lock().await.events.subscribe(16))
    }
}

async fn serve(stream: local::Stream, timers: Arc<Timers>) -> Result<(), Box<dyn std::error::Error>> {
    let (reader, writer) = tokio::io::split(stream);
    // Each connection gets an RPC server calling the same shared Timers object.
    // new() also creates a transferable client handle; the object stays here.
    let (server, client) = TimerServiceServerShared::<_, codec::Default>::new(timers);
    // provide() establishes Remoc over the stream, sends the client handle as
    // the initial value, and starts the connection's background I/O task.
    // It returns after sending that value, not when the connection ends.
    remoc::Connect::io(remoc::Cfg::default(), reader, writer).provide(client).await?;
    // Transporting bytes and handling RPC calls are separate: serve() receives
    // method calls and invokes our TimerService implementation.
    server.serve().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut listener = local::Listener::bind(&args.endpoint)?;
    let timers = Arc::new(Timers::default());
    println!("Listening on {}. Press Ctrl+C to stop.", args.endpoint);

    loop {
        tokio::select! {
            stream = listener.accept() => {
                let stream = stream?;
                let timers = timers.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve(stream, timers).await {
                        eprintln!("Client connection failed: {err}");
                    }
                });
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
        }
    }

    Ok(())
}
