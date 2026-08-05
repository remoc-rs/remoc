//! Emulated network link with configurable round-trip time and bandwidth.
//!
//! Shaping happens on the sending side in two concurrent stages: a token bucket
//! paces bytes onto the link, then each chunk is held for half the round-trip time
//! before it reaches the socket. The stages are separate tasks, so many chunks are
//! in flight at once. A shaper that instead slept before every write would serialize
//! the pipeline and cap throughput at one chunk per round-trip, measuring itself
//! rather than the protocol under test.

use bytes::Bytes;
use std::{
    net::Ipv4Addr,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
};
use tokio::{
    io::{self, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    time::{Duration, Instant, sleep, sleep_until},
};

/// Amount of data the shaper moves at once.
const CHUNK: usize = 16_384;

/// Measured overshoot of [`sleep_until`], see [`calibrate`].
static SLEEP_OVERSHOOT: OnceLock<Duration> = OnceLock::new();

/// Measures how late [`sleep_until`] fires and remembers it.
///
/// Tokio's timer has a granularity of one millisecond and never fires early, so an
/// uncompensated delay stage adds roughly that much to every direction. The overshoot is
/// therefore measured once and subtracted from each delay, leaving jitter around the
/// target instead of a systematic offset. Delays below the overshoot cannot be
/// compensated, so the emulator has a round-trip time floor of twice the overshoot;
/// see [`rtt_floor`].
pub async fn calibrate() {
    const ROUNDS: u32 = 64;
    const NOMINAL: Duration = Duration::from_micros(2_000);

    let mut total = Duration::ZERO;
    for _ in 0..ROUNDS {
        let target = Instant::now() + NOMINAL;
        sleep_until(target).await;
        total += Instant::now().saturating_duration_since(target);
    }

    let _ = SLEEP_OVERSHOOT.set(total / ROUNDS);
}

/// The calibrated sleep overshoot.
pub fn sleep_overshoot() -> Duration {
    SLEEP_OVERSHOOT.get().copied().unwrap_or_default()
}

/// Smallest non-zero round-trip time the emulator can reproduce.
///
/// A link must either use no delay at all or a round-trip time well above this.
pub fn rtt_floor() -> Duration {
    2 * sleep_overshoot()
}

/// Properties of an emulated link.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// Descriptive name.
    pub name: &'static str,
    /// Round-trip time; half of it is applied to each direction.
    pub rtt: Duration,
    /// Bandwidth limit per direction in bytes per second.
    ///
    /// Every link is limited. Over an unlimited loopback the transport is a memory
    /// copy, so the baseline measures how fast the machine can move bytes and every
    /// protocol on top of it can only lose; the comparison would say nothing about
    /// behaviour on a real network.
    pub rate: u64,
}

impl Link {
    /// Defines a link by round-trip time in milliseconds and bandwidth in MB/s.
    pub const fn new(name: &'static str, rtt_ms: u64, rate_mb: u64) -> Self {
        Self { name, rtt: Duration::from_millis(rtt_ms), rate: rate_mb * 1_000_000 }
    }

    /// Bandwidth-delay product in bytes.
    pub fn bdp(&self) -> u64 {
        (self.rate as f64 * self.rtt.as_secs_f64()) as u64
    }

    /// Queue in front of the bottleneck, holding one bandwidth-delay product.
    fn queue_capacity(&self) -> usize {
        (self.bdp() as usize).max(256 * 1024)
    }

    /// Token bucket burst, worth 10 ms of traffic.
    fn burst(&self) -> f64 {
        (self.rate as f64 * 0.01).max(4.0 * CHUNK as f64)
    }
}

/// Establishes a TCP connection over loopback and shapes both directions.
pub async fn connect(link: Link) -> io::Result<(LinkSide, LinkSide)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;

    let accept = tokio::spawn(async move { listener.accept().await });
    let client = TcpStream::connect(addr).await?;
    let (server, _) = accept.await??;

    Ok((shape(client, link), shape(server, link)))
}

/// One end of an emulated link.
pub struct LinkSide {
    pub reader: OwnedReadHalf,
    pub writer: LinkWriter,
}

fn shape(socket: TcpStream, link: Link) -> LinkSide {
    let _ = socket.set_nodelay(true);
    let (reader, writer) = socket.into_split();

    let (app_side, link_side) = io::duplex(link.queue_capacity());
    let (departures_tx, departures_rx) = mpsc::unbounded_channel();

    tokio::spawn(pace(link_side, departures_tx, link.rate, link.burst()));
    tokio::spawn(propagate(departures_rx, writer, link.rtt / 2));

    LinkSide { reader, writer: LinkWriter(app_side) }
}

/// Serves the queue at the configured bandwidth.
async fn pace(
    mut queue: DuplexStream, departures: mpsc::UnboundedSender<(Instant, Bytes)>, rate: u64, burst: f64,
) {
    let mut bucket = TokenBucket::new(rate, burst);
    let mut buf = vec![0u8; CHUNK];

    loop {
        let n = match queue.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        bucket.consume(n).await;

        if departures.send((Instant::now(), Bytes::copy_from_slice(&buf[..n]))).is_err() {
            break;
        }
    }
}

/// Delivers served chunks after the propagation delay.
async fn propagate(
    mut departures: mpsc::UnboundedReceiver<(Instant, Bytes)>, mut output: OwnedWriteHalf, delay: Duration,
) {
    // Compensating a delay that is shorter than the overshoot would make it negative,
    // so such a delay is left alone and accounted for by the round-trip time floor.
    let overshoot = sleep_overshoot();
    let delay = if delay > overshoot { delay - overshoot } else { delay };

    while let Some((departure, chunk)) = departures.recv().await {
        // An elapsed deadline must not touch the timer, which would round it up to the
        // next tick and reintroduce the very delay that was just compensated away.
        let release = departure + delay;
        if release > Instant::now() {
            sleep_until(release).await;
        }
        if output.write_all(&chunk).await.is_err() {
            break;
        }
    }

    let _ = output.shutdown().await;
}

struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    updated: Instant,
}

impl TokenBucket {
    fn new(rate: u64, burst: f64) -> Self {
        // The bucket starts empty. A full one would let a transfer begin with a burst of
        // up to `burst` bytes at unlimited speed, which short runs would report as a rate
        // above the configured one.
        Self { rate: rate as f64, burst, tokens: 0.0, updated: Instant::now() }
    }

    async fn consume(&mut self, amount: usize) {
        let amount = amount as f64;

        loop {
            let now = Instant::now();
            self.tokens =
                (self.tokens + now.duration_since(self.updated).as_secs_f64() * self.rate).min(self.burst);
            self.updated = now;

            if self.tokens >= amount {
                self.tokens -= amount;
                return;
            }

            sleep(Duration::from_secs_f64((amount - self.tokens) / self.rate)).await;
        }
    }
}

/// Sending half of an emulated link.
pub struct LinkWriter(DuplexStream);

impl AsyncWrite for LinkWriter {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}
