//! A command-line client for the timer daemon.

use clap::{Parser, Subcommand};
use remoc::prelude::*;
use std::{io::Write, time::Duration};
use timer_daemon_example::{NewTimer, TimerService, TimerServiceClient, local};

#[derive(Parser)]
#[command(about = "Schedule and watch reminders in the local timer daemon")]
struct Args {
    /// Unix socket path or Windows named-pipe name.
    #[arg(long, default_value = local::DEFAULT_ENDPOINT)]
    endpoint: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Schedule a reminder, for example: add 30s "Check tea".
    Add {
        /// Delay such as 30s, 5m or 1h.
        #[arg(value_parser = humantime::parse_duration)]
        after: Duration,
        /// Reminder message.
        message: String,
        /// Wait for this timer to finish.
        #[arg(long)]
        wait: bool,
    },
    /// List pending reminders.
    List,
    /// Print future timer completions until Ctrl+C.
    Watch,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let stream = local::connect(&args.endpoint).await?;
    let (reader, writer) = tokio::io::split(stream);
    // consume() establishes Remoc and receives the initial value sent by the
    // daemon's provide(): our typed RPC client. It also starts the connection's
    // background I/O task, carrying both RPC traffic and transferred channels.
    let client: TimerServiceClient = remoc::Connect::io(remoc::Cfg::default(), reader, writer).consume().await?;

    match args.command {
        Command::Add { after, message, wait } => {
            let (notify, completion) = if wait {
                // Keep the receiver here and send its sender to the daemon.
                // Unlike a Tokio oneshot, this Remoc endpoint can cross the
                // connection, including as a field in the NewTimer struct.
                let (sender, receiver) = rch::oneshot::channel();
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };
            // This looks like an ordinary trait call, but sends the request to
            // timerd. The RPC returns the id, not the eventual completion.
            let id = client.add(NewTimer { after, message, notify }).await?;
            println!("Added timer {id}");
            if let Some(completion) = completion {
                // The RPC is finished. Await a separate notification through
                // our oneshot receiver, still using the same connection.
                std::io::stdout().flush()?;
                tokio::select! {
                    timer = completion => {
                        let timer = timer?;
                        println!("Finished {}: {}", timer.id, timer.message);
                    }
                    result = tokio::signal::ctrl_c() => {
                        result?;
                    }
                }
            }
        }
        Command::List => {
            let timers = client.list().await?;
            if timers.is_empty() {
                println!("No pending timers.");
            }
            for timer in timers {
                println!("{}: {}", timer.id, timer.message);
            }
        }
        Command::Watch => {
            // Here the direction is reversed: the daemon returns a receiver
            // as the RPC result and keeps the broadcast sender.
            let mut events = client.subscribe().await?;
            println!("Watching timer completions. Press Ctrl+C to stop.");
            std::io::stdout().flush()?;
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let timer = event?;
                        println!("Finished {}: {}", timer.id, timer.message);
                    }
                    result = tokio::signal::ctrl_c() => {
                        result?;
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
