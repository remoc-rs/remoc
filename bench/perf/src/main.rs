//! Throughput benchmark for Remoc.
//!
//! Runs a stack of layers over an emulated network link and reports how much of the
//! raw TCP throughput each layer retains. The purpose is to show what Remoc costs in
//! practice, not to gate anything in CI: both endpoints share one machine, so all
//! numbers include the peer's work and the emulator's own overhead.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -- --help
//! ```

mod layers;
mod link;

use clap::Parser;
use serde_json::{Value, json};
use std::{collections::HashMap, error::Error, fs, future::Future, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::Instant,
};

use layers::{CodecKind, Layer};
use link::{Link, connect};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Throughput benchmark for Remoc.
#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// Shorten every run to one second instead of five, trading accuracy for time.
    #[arg(long)]
    quick: bool,

    /// Run only these layers, by name; all of them by default.
    ///
    /// The first selected layer is the baseline the others are reported against.
    #[arg(long = "layer", value_name = "NAME", value_delimiter = ',')]
    layers: Vec<String>,

    /// Run only these links, by name; all of them by default.
    #[arg(long = "link", value_name = "NAME", value_delimiter = ',')]
    links: Vec<String>,

    /// Run only these message sizes, in bytes; all of them by default.
    #[arg(long = "size", value_name = "BYTES", value_delimiter = ',')]
    sizes: Vec<usize>,

    /// List the available layer names, link names and message sizes, then exit.
    #[arg(long)]
    list: bool,

    /// Skip the check that the link emulator reproduces its configuration.
    #[arg(long)]
    skip_validation: bool,

    /// Write the results as JSON to this path.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

impl Args {
    /// The layers to run, in the order they are defined.
    fn selected_layers(&self) -> Result<Vec<Layer>> {
        select(&self.layers, &Layer::ALL, |layer| layer.name(), "layer")
    }

    /// The links to run over, in the order they are defined.
    fn selected_links(&self) -> Result<Vec<Link>> {
        select(&self.links, LINKS, |link| link.name.to_string(), "link")
    }

    /// The message sizes to sweep, in the order they are defined.
    fn selected_sizes(&self) -> Result<Vec<usize>> {
        select(
            &self.sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            MSG_SIZES,
            |s| s.to_string(),
            "size",
        )
    }
}

/// Picks the `available` items named by `wanted`, or all of them if nothing is wanted.
///
/// An unknown name is an error rather than an empty selection, so that a typo during a
/// quick iteration does not silently measure nothing.
fn select<T: Copy>(
    wanted: &[String], available: &[T], name: impl Fn(&T) -> String, what: &str,
) -> Result<Vec<T>> {
    if wanted.is_empty() {
        return Ok(available.to_vec());
    }

    for want in wanted {
        if !available.iter().any(|item| &name(item) == want) {
            let names: Vec<_> = available.iter().map(&name).collect();
            return Err(format!("unknown {what} `{want}`; available are: {}", names.join(", ")).into());
        }
    }

    Ok(available.iter().filter(|item| wanted.contains(&name(item))).copied().collect())
}

/// Emulated links, from fast to hostile.
///
/// Every link is bandwidth limited. An unlimited loopback would compare Remoc against a
/// memory copy, which says nothing about behaviour on a real network.
///
/// Round-trip times must be either zero or well above the emulator's floor of roughly
/// two milliseconds; see [`link::rtt_floor`]. A gigabit LAN is therefore modelled as pure
/// bandwidth limiting, its sub-millisecond latency being below what can be reproduced.
const LINKS: &[Link] =
    &[Link::new("lan", 0, 125), Link::new("wifi", 10, 12), Link::new("lte", 50, 6), Link::new("wan", 100, 25)];

/// Payload sizes to sweep.
///
/// Throughput saturates the link somewhere around a kilobyte, so the sweep is dense
/// below that. The last size is the only one above [`Cfg::chunk_size`](remoc::Cfg) and
/// covers bulk transfer.
const MSG_SIZES: &[usize] = &[64, 256, 512, 1_024, 4_096, 65_536];

/// How long each measured transfer runs.
///
/// Every run is bounded by time alone, so that a fast and a slow layer are both measured
/// over the same interval and the reported rates are directly comparable.
const RUN_LIMIT: Duration = Duration::from_secs(5);

/// [`RUN_LIMIT`] in quick mode.
const QUICK_RUN_LIMIT: Duration = Duration::from_secs(1);

/// How long the raw TCP transfer of the link validation runs.
///
/// Long enough that the initial fill of the link is amortized.
const VALIDATION_LIMIT: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list {
        list();
        return Ok(());
    }

    let layers = args.selected_layers()?;
    let links = args.selected_links()?;
    let sizes = args.selected_sizes()?;

    let mut report = json!({
        "quick": args.quick,
        "run_limit_secs": if args.quick { QUICK_RUN_LIMIT } else { RUN_LIMIT }.as_secs_f64(),
        "baseline_layer": layers.first().ok_or("no layer selected")?.name(),
    });

    isolated(link::calibrate());
    report["sleep_overshoot_ms"] = json!(link::sleep_overshoot().as_secs_f64() * 1e3);
    report["rtt_floor_ms"] = json!(link::rtt_floor().as_secs_f64() * 1e3);
    report["sample_bytes"] =
        json!(CodecKind::ALL.iter().map(|c| (c.name(), c.sample_bytes())).collect::<HashMap<_, _>>());

    if !args.skip_validation {
        report["validation"] = validate(&links)?;
    }
    report["runs"] = measure(&layers, &links, &sizes, args.quick)?;

    if let Some(out) = args.out {
        fs::write(&out, serde_json::to_string_pretty(&report)?)?;
        println!("\nWrote {}", out.display());
    }

    println!(
        "\nBoth endpoints run in this process, so every result includes the peer's work \n\
         and, on shaped links, the emulator's own CPU cost. Treat the numbers as a lower \n\
         bound on what Remoc achieves between separate machines."
    );

    Ok(())
}

/// Runs `future` on a runtime of its own, once everything it started has stopped.
///
/// A run does not end when its transfer does: the connection is still closing and
/// whatever is buffered is still draining, which on a slow link takes hundreds of
/// milliseconds. Sharing one runtime across the sweep let that teardown overlap the next
/// measurement, which then reported the cost of its predecessor and made results depend
/// on the order they were taken in. Waiting for the tasks to finish and giving each run
/// its own runtime is what makes a measurement independent of the ones before it, and it
/// charges the teardown to the run that caused it.
fn isolated<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Runtime::new().expect("cannot build runtime");
    let result = runtime.block_on(future);

    assert_torn_down(&runtime);

    // Nothing is left for this to cancel; it only bounds the damage should the check
    // above ever be relaxed.
    runtime.shutdown_timeout(Duration::from_secs(5));

    result
}

/// How long the tasks of a finished run may take to end by themselves.
///
/// Teardown is not instant and legitimately so: chmux closes a connection gracefully,
/// which costs a few round-trips, and whatever is still buffered drains through the link
/// at its configured rate. On the emulated LTE link that was measured at up to 0.4 s for
/// 64 KiB messages, against 4 ms for the same transfer over plain TCP. The bound is
/// therefore far above any legitimate teardown, since its purpose is to catch a task that
/// never ends at all rather than one that takes a while.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// Panics unless every task of the finished run has ended by itself.
///
/// Nothing owns a layer once its transfer is over, so everything it started must stop on
/// its own: dropping both halves of a channel must end the tasks driving it, dropping the
/// last channel must end the chmux dispatcher, and the link emulator's shapers must
/// follow their closed queues. A task still running here has no way to ever be stopped
/// and would keep consuming CPU for the rest of the process, so it is a bug worth failing
/// on rather than something for the runtime shutdown to hide.
fn assert_torn_down(runtime: &tokio::runtime::Runtime) {
    let deadline = std::time::Instant::now() + TEARDOWN_GRACE;

    loop {
        // Blocking tasks are not covered: their metrics need `tokio_unstable`, and unlike
        // the tasks watched here they are bounded units of work rather than loops that
        // could keep running for the rest of the process.
        let tasks = runtime.metrics().num_alive_tasks();

        if tasks == 0 {
            return;
        }

        if std::time::Instant::now() >= deadline {
            panic!(
                "{tasks} task(s) were still running {TEARDOWN_GRACE:?} after the transfer ended.\n\
                 Everything a run starts has to stop once its channels are dropped, so this\n\
                 is a teardown bug. Left running, such a task would steal CPU from every\n\
                 later measurement in this process."
            );
        }

        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Prints what `--layer`, `--link` and `--size` accept.
fn list() {
    println!("Layers (--layer):");
    for layer in Layer::ALL {
        println!("  {:<32} {}", layer.name(), layer.description());
    }

    println!("\nLinks (--link):");
    for link in LINKS {
        println!("  {:<32} rtt {:?}, {} MB/s", link.name, link.rtt, link.rate / 1_000_000);
    }

    println!("\nMessage sizes (--size):");
    println!("  {}", MSG_SIZES.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "));
}

/// Checks that the link emulator actually reproduces its configuration.
///
/// Nothing measured on top of the emulator is meaningful unless this holds.
fn validate(links: &[Link]) -> Result<Value> {
    println!("Link emulator validation (rtt floor {:.2}ms)", link::rtt_floor().as_secs_f64() * 1e3);
    println!("{:<12} {:>12} {:>12} {:>12} {:>12}", "link", "rtt cfg", "rtt meas", "rate cfg", "rate meas");

    let mut results = Vec::new();

    for link in links {
        let rtt = isolated(measure_rtt(*link))?;

        let outcome = isolated(layers::run(Layer::RawTcp, *link, 1_048_576, VALIDATION_LIMIT))?;
        let rate = outcome.mbytes_per_sec();

        let rtt_cfg = link.rtt.as_secs_f64() * 1e3;
        let rate_cfg = link.rate as f64 / 1e6;

        println!(
            "{:<12} {:>10.2}ms {:>10.2}ms {:>9.0}MB/s {:>9.1}MB/s",
            link.name,
            rtt_cfg,
            rtt * 1e3,
            rate_cfg,
            rate,
        );

        results.push(json!({
            "link": link.name,
            "rtt_ms_configured": rtt_cfg,
            "rtt_ms_measured": rtt * 1e3,
            "mbytes_per_s_configured": rate_cfg,
            "mbytes_per_s_measured": rate,
        }));
    }

    println!();
    Ok(Value::Array(results))
}

/// Measures the round-trip time of an emulated link by ping-pong over raw TCP.
async fn measure_rtt(link: Link) -> Result<f64> {
    const ROUNDS: usize = 20;

    let (client, server) = connect(link).await?;
    let (mut client_reader, mut client_writer) = (client.reader, client.writer);
    let (mut server_reader, mut server_writer) = (server.reader, server.writer);

    tokio::spawn(async move {
        let mut buf = [0u8; 1];
        while server_reader.read_exact(&mut buf).await.is_ok() {
            if server_writer.write_all(&buf).await.is_err() {
                break;
            }
            let _ = server_writer.flush().await;
        }
    });

    let mut buf = [0u8; 1];
    let mut total = Duration::ZERO;

    for round in 0..=ROUNDS {
        let started = Instant::now();
        client_writer.write_all(b"p").await?;
        client_writer.flush().await?;
        client_reader.read_exact(&mut buf).await?;

        // The first round is discarded as warm-up.
        if round > 0 {
            total += started.elapsed();
        }
    }

    Ok(total.as_secs_f64() / ROUNDS as f64)
}

/// Runs the selected matrix of links, payload sizes and layers.
///
/// Every result is also reported as a fraction of the first selected layer, which is the
/// baseline: with the full selection that is plain TCP, and with a subset it is whatever
/// the subset starts with.
fn measure(layers: &[Layer], links: &[Link], sizes: &[usize], quick: bool) -> Result<Value> {
    let limit = if quick { QUICK_RUN_LIMIT } else { RUN_LIMIT };
    let baseline_layer = layers.first().ok_or("no layer selected")?;

    let mut results = Vec::new();

    for link in links {
        println!("Link {} (rtt {:?}, rate {:?} B/s)", link.name, link.rtt, link.rate);
        println!(
            "{:<40} {:>10} {:>12} {:>12} {:>10} {:>10} {:>12}",
            "layer",
            "msg size",
            "MB/s",
            "msgs/s",
            format!("of {}", baseline_layer.name()),
            "CPU s/GB",
            "records/s"
        );

        for &msg_size in sizes {
            let mut baseline = None;

            for &layer in layers {
                let cpu_before = cpu_time();
                let outcome = isolated(layers::run(layer, *link, msg_size, limit))?;
                let cpu = cpu_time() - cpu_before;

                let rate = outcome.mbytes_per_sec();
                let baseline = *baseline.get_or_insert(rate);
                let cpu_per_gb = cpu / (outcome.bytes as f64 / 1e9);

                // Codecs differ in how compactly they encode a record, so encoded bytes
                // per second understate a compact codec: it moves the same information in
                // fewer bytes. Records per second is what compares codecs fairly.
                let records_per_s =
                    layer.codec().map(|c| outcome.bytes as f64 / c.sample_bytes() as f64 / outcome.secs);
                let cpu_per_mrecords = records_per_s.map(|records| cpu / (records * outcome.secs / 1e6));

                println!(
                    "{:<40} {:>10} {:>12.1} {:>12.0} {:>9.0}% {:>10.2} {:>12}",
                    layer.description(),
                    msg_size,
                    rate,
                    outcome.msgs_per_sec(),
                    rate / baseline * 100.0,
                    cpu_per_gb,
                    match records_per_s {
                        Some(records) => format!("{records:.0}"),
                        None => "-".to_string(),
                    },
                );

                results.push(json!({
                    "link": link.name,
                    "rtt_ms": link.rtt.as_secs_f64() * 1e3,
                    "mbytes_per_s_limit": link.rate as f64 / 1e6,
                    "layer": layer.name(),
                    "layer_description": layer.description(),
                    "codec": layer.codec().map(|c| c.name()),
                    "remote_buffer": layer.remote_buffer().map(|b| b.items()),
                    "msg_size": msg_size,
                    "msgs": outcome.msgs,
                    "bytes": outcome.bytes,
                    "secs": outcome.secs,
                    "mbytes_per_s": rate,
                    "msgs_per_s": outcome.msgs_per_sec(),
                    "records_per_s": records_per_s,
                    "fraction_of_baseline": rate / baseline,
                    "cpu_secs_per_gbyte": cpu_per_gb,
                    "cpu_secs_per_mrecords": cpu_per_mrecords,
                }));
            }

            println!();
        }
    }

    Ok(Value::Array(results))
}

/// CPU time of this process in seconds, zero where unavailable.
fn cpu_time() -> f64 {
    #[cfg(target_os = "linux")]
    {
        // Field 14 (utime) and 15 (stime) of /proc/self/stat, in USER_HZ (100 Hz).
        let read = || -> Option<f64> {
            let stat = fs::read_to_string("/proc/self/stat").ok()?;
            let after_comm = &stat[stat.rfind(')')? + 2..];
            let fields: Vec<&str> = after_comm.split_whitespace().collect();
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            Some((utime + stime) as f64 / 100.0)
        };
        read().unwrap_or_default()
    }

    #[cfg(not(target_os = "linux"))]
    0.0
}
