//! The benchmarked layers.
//!
//! Each layer adds exactly one thing to the layer below it, so the difference
//! between two adjacent results is attributable to that one addition.
//!
//! Every Remoc layer is established with [`Connect::io`] over the link, which is how
//! an application would use it.

use bytes::{Buf, Bytes};
use remoc::{Cfg, Connect, RemoteSend, chmux, codec, prelude::*};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    io,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};

use crate::link::{Link, LinkSide, connect};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Byte pattern used for payloads.
const FILL: u8 = 0xa5;

/// Local item buffer of the [`rch::mpsc`] channel, as passed to [`rch::mpsc::channel`].
///
/// This is the buffer between the sender and the connection on the sending side. It is
/// unrelated to [`RemoteBuffer`], which is what the receiving side allocates.
const MPSC_BUFFER: usize = 128;

/// Write buffer of the raw TCP baseline, matching [`Cfg::io_buffer_size`](remoc::Cfg).
///
/// Remoc buffers its writes, so an unbuffered baseline would be compared against it on
/// unequal terms: at small message sizes it would spend all its time on write calls
/// rather than on the transport, and the comparison would measure that instead of what
/// Remoc costs.
const TCP_BUFFER: usize = 65_536;

/// Item buffer the receiving side of an [`rch::mpsc`] channel allocates once the channel
/// half arrives there.
///
/// Set through [`MpscExt::with_buffer`] as a const generic on the transferred half, so it
/// is a property of the type rather than a runtime argument. It bounds how many items may
/// be in flight towards the receiver, and thus how much the sender may run ahead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteBuffer {
    /// [`rch::DEFAULT_BUFFER`] items, whatever the channel does without being asked.
    Default,
    /// [`RemoteBuffer::LARGE`] items.
    Large,
}

impl RemoteBuffer {
    /// Item count of [`RemoteBuffer::Large`].
    pub const LARGE: usize = 32;

    pub fn items(&self) -> usize {
        match self {
            Self::Default => rch::DEFAULT_BUFFER,
            Self::Large => Self::LARGE,
        }
    }

    /// Name suffix distinguishing this buffer from the default, which carries none.
    fn suffix(&self) -> String {
        match self {
            Self::Default => String::new(),
            Self::Large => format!("_buf{}", Self::LARGE),
        }
    }
}

/// A codec the struct layers are run with.
///
/// The byte layers are not parameterized by it: they carry [`Bytes`], which every codec
/// passes through unchanged, so a sweep over codecs would repeat the same measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodecKind {
    /// [`codec::Postbag`], the default codec, encoding field identifiers.
    Postbag,
    /// [`codec::PostbagSlim`], dropping field identifiers.
    PostbagSlim,
    /// [`codec::Bincode`].
    Bincode,
}

impl CodecKind {
    pub const ALL: [Self; 3] = [Self::Postbag, Self::PostbagSlim, Self::Bincode];

    pub fn name(&self) -> &'static str {
        match self {
            Self::Postbag => "postbag",
            Self::PostbagSlim => "postbag_slim",
            Self::Bincode => "bincode",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::Postbag => "postbag",
            Self::PostbagSlim => "postbag slim",
            Self::Bincode => "bincode",
        }
    }

    /// Encoded size of one [`Sample`] under this codec.
    pub fn sample_bytes(&self) -> usize {
        match self {
            Self::Postbag => Sample::encoded_len::<codec::Postbag>(),
            Self::PostbagSlim => Sample::encoded_len::<codec::PostbagSlim>(),
            Self::Bincode => Sample::encoded_len::<codec::Bincode>(),
        }
    }
}

/// A benchmarked layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Plain TCP, the baseline every other layer is measured against.
    RawTcp,
    /// A raw [`chmux`] port: adds multiplexing, framing and flow control.
    Chmux,
    /// A [`rch::base`] channel of [`Bytes`]: adds serialization.
    Base,
    /// A [`rch::mpsc`] channel of [`Bytes`]: adds the channel layer.
    Mpsc,
    /// Plain TCP carrying the same encoded records as [`Layer::MpscStruct`].
    ///
    /// Encoding a struct costs the same whatever moves the bytes afterwards, so this is
    /// the baseline the struct layer has to be read against; against plain TCP it would
    /// be charged for Serde's work as well as for Remoc's.
    TcpStruct(CodecKind),
    /// A [`rch::mpsc`] channel of a batch of records: adds realistic codec work.
    ///
    /// Carries the buffer the receiving side allocates, which decides how far the sender
    /// may run ahead of the receiver.
    MpscStruct(CodecKind, RemoteBuffer),
    /// A [`rch::mpsc`] channel spreading its items over additional transfer channels.
    ///
    /// Carries the number of *additional* channels, so zero is a single channel and thus
    /// the behaviour before the feature existed, which is also the library default. Above
    /// that, consecutive items are serialized and deserialized on separate tasks that can
    /// run on separate cores; order is preserved because both endpoints round-robin in
    /// lockstep. Everything else matches [`Layer::MpscStruct`], so the two are comparable.
    MpscStructParallel(CodecKind, usize),
}

impl Layer {
    pub const ALL: [Self; 28] = [
        Self::RawTcp,
        Self::Chmux,
        Self::Base,
        Self::Mpsc,
        Self::TcpStruct(CodecKind::Postbag),
        Self::MpscStruct(CodecKind::Postbag, RemoteBuffer::Default),
        Self::MpscStruct(CodecKind::Postbag, RemoteBuffer::Large),
        Self::MpscStructParallel(CodecKind::Postbag, 0),
        Self::MpscStructParallel(CodecKind::Postbag, 1),
        Self::MpscStructParallel(CodecKind::Postbag, 2),
        Self::MpscStructParallel(CodecKind::Postbag, 3),
        Self::MpscStructParallel(CodecKind::Postbag, 4),
        Self::TcpStruct(CodecKind::PostbagSlim),
        Self::MpscStruct(CodecKind::PostbagSlim, RemoteBuffer::Default),
        Self::MpscStruct(CodecKind::PostbagSlim, RemoteBuffer::Large),
        Self::MpscStructParallel(CodecKind::PostbagSlim, 0),
        Self::MpscStructParallel(CodecKind::PostbagSlim, 1),
        Self::MpscStructParallel(CodecKind::PostbagSlim, 2),
        Self::MpscStructParallel(CodecKind::PostbagSlim, 3),
        Self::MpscStructParallel(CodecKind::PostbagSlim, 4),
        Self::TcpStruct(CodecKind::Bincode),
        Self::MpscStruct(CodecKind::Bincode, RemoteBuffer::Default),
        Self::MpscStruct(CodecKind::Bincode, RemoteBuffer::Large),
        Self::MpscStructParallel(CodecKind::Bincode, 0),
        Self::MpscStructParallel(CodecKind::Bincode, 1),
        Self::MpscStructParallel(CodecKind::Bincode, 2),
        Self::MpscStructParallel(CodecKind::Bincode, 3),
        Self::MpscStructParallel(CodecKind::Bincode, 4),
    ];

    pub fn name(&self) -> String {
        match self {
            Self::RawTcp => "raw_tcp".into(),
            Self::Chmux => "chmux".into(),
            Self::Base => "base".into(),
            Self::Mpsc => "mpsc".into(),
            Self::TcpStruct(codec) => format!("tcp_struct_{}", codec.name()),
            Self::MpscStruct(codec, buffer) => format!("mpsc_struct{}_{}", buffer.suffix(), codec.name()),
            Self::MpscStructParallel(codec, parallel) => {
                format!("mpsc_struct_par{parallel}_{}", codec.name())
            }
        }
    }

    pub fn description(&self) -> String {
        match self {
            Self::RawTcp => "plain TCP".into(),
            Self::Chmux => "chmux port, raw bytes".into(),
            Self::Base => "rch::base, Bytes".into(),
            Self::Mpsc => "rch::mpsc, Bytes".into(),
            Self::TcpStruct(codec) => format!("plain TCP, structs ({})", codec.description()),
            Self::MpscStruct(codec, RemoteBuffer::Default) => {
                format!("rch::mpsc, structs ({})", codec.description())
            }
            Self::MpscStruct(codec, buffer) => {
                format!("rch::mpsc, structs ({}, buffer {})", codec.description(), buffer.items())
            }
            Self::MpscStructParallel(codec, parallel) => {
                format!("rch::mpsc, structs ({}, parallel {parallel})", codec.description())
            }
        }
    }

    /// The codec the layer is run with, if it carries structs.
    pub fn codec(&self) -> Option<CodecKind> {
        match self {
            Self::TcpStruct(codec) | Self::MpscStruct(codec, _) | Self::MpscStructParallel(codec, _) => {
                Some(*codec)
            }
            _ => None,
        }
    }

    /// The number of additional transfer channels, if the layer sets it.
    pub fn parallel(&self) -> Option<usize> {
        match self {
            Self::MpscStructParallel(_, parallel) => Some(*parallel),
            _ => None,
        }
    }

    /// The buffer the receiving side allocates, if the layer is an [`rch::mpsc`] channel
    /// whose half is transferred.
    pub fn remote_buffer(&self) -> Option<RemoteBuffer> {
        match self {
            Self::MpscStruct(_, buffer) => Some(*buffer),
            _ => None,
        }
    }
}

/// Result of one transfer.
#[derive(Clone, Copy, Debug)]
pub struct Outcome {
    /// Payload bytes received, excluding any protocol overhead.
    pub bytes: u64,
    /// Messages received.
    pub msgs: u64,
    /// Wall time of the transfer.
    pub secs: f64,
}

impl Outcome {
    pub fn mbytes_per_sec(&self) -> f64 {
        self.bytes as f64 / self.secs / 1_000_000.0
    }

    pub fn msgs_per_sec(&self) -> f64 {
        self.msgs as f64 / self.secs
    }
}

/// Times a transfer for a fixed interval.
///
/// A run is bounded by wall-clock time alone: whatever was transferred until the limit
/// is reported. The bound is enforced by cancelling the receive loop with
/// [`tokio::time::timeout`] rather than by polling the clock, so it holds for a layer of
/// any speed without a check on the hot path.
struct Meter {
    started: Instant,
    bytes: u64,
    count: u64,
}

impl Meter {
    fn new() -> Self {
        Self { started: Instant::now(), bytes: 0, count: 0 }
    }

    /// Records a received message.
    fn record(&mut self, bytes: usize) {
        self.bytes += bytes as u64;
        self.count += 1;
    }

    fn finish(self) -> Outcome {
        Outcome { bytes: self.bytes, msgs: self.count, secs: self.started.elapsed().as_secs_f64() }
    }
}

/// Runs the receive loop of a layer until `limit` has elapsed.
///
/// The loop is cancelled at the limit, which is why every layer's receive side must be
/// safe to drop mid-await; all of them are, being plain reads.
macro_rules! receive_for {
    ($limit:expr, $meter:ident, $body:block) => {{
        let mut $meter = Meter::new();

        // A layer whose sender ends or fails leaves the loop early; that is reported.
        // Reaching the limit is the normal case and not an error.
        if let Ok(result) = tokio::time::timeout($limit, async {
            {
                $body
            }
            Result::<()>::Ok(())
        })
        .await
        {
            result?;
        }

        $meter.finish()
    }};
}

/// A timestamped reading, representing realistic message content.
///
/// A message carries a batch of these rather than one grown record, so that the ratio
/// of scalar fields to bulk data stays the same at every message size and the layer
/// measures codec work instead of the shape of one fixture. The fields are chosen so
/// that the record encodes to 64 bytes and a batch hits every swept size exactly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub id: u64,
    pub timestamp: u64,
    pub source: String,
    pub valid: bool,
    pub value: f64,
}

impl Sample {
    fn new() -> Self {
        Self { id: 90_000, timestamp: 1_700_000_000, source: "sensor01".to_string(), valid: true, value: 1.0 }
    }

    /// Encoded size of one sample under codec `C`.
    ///
    /// Taken from the codec rather than assumed, so that the batch size and the reported
    /// payload bytes match what actually goes over the wire. Codecs differ in how much
    /// they encode, so this is per codec.
    pub fn encoded_len<C: codec::Codec>() -> usize {
        // The codec refuses to run outside a connection, because the data format version
        // is normally negotiated with the peer. Sizing a sample involves no peer, so the
        // local version is used, which is what two current endpoints agree on anyway.
        let allowed = codec::ALLOW_OUTSIDE_REMOC.replace(true);
        let mut buf = Vec::new();
        <C as codec::Codec>::serialize(&mut buf, &Self::new()).expect("sample is serializable");
        codec::ALLOW_OUTSIDE_REMOC.set(allowed);
        buf.len()
    }

    /// Builds a batch of `size` encoded bytes under codec `C`.
    fn batch<C: codec::Codec>(size: usize) -> Vec<Self> {
        vec![Self::new(); (size / Self::encoded_len::<C>()).max(1)]
    }
}

/// Dispatches to a codec-generic layer implementation.
///
/// Further generic arguments, such as a buffer size that has to be a constant, may be
/// given in a turbofish and are passed on after the codec.
macro_rules! with_codec {
    ($codec:expr, $func:ident $(::<$($generic:tt),*>)? ($($arg:expr),*)) => {
        match $codec {
            CodecKind::Postbag => $func::<codec::Postbag $($(, $generic)*)?>($($arg),*).await,
            CodecKind::PostbagSlim => $func::<codec::PostbagSlim $($(, $generic)*)?>($($arg),*).await,
            CodecKind::Bincode => $func::<codec::Bincode $($(, $generic)*)?>($($arg),*).await,
        }
    };
}

/// Transfers messages of `msg_size` bytes over `link` using `layer` for `limit`.
pub async fn run(layer: Layer, link: Link, msg_size: usize, limit: Duration) -> Result<Outcome> {
    match layer {
        Layer::RawTcp => raw_tcp(link, msg_size, limit).await,
        Layer::Chmux => chmux_port(link, msg_size, limit).await,
        Layer::Base => base(link, msg_size, limit).await,
        Layer::Mpsc => mpsc(link, msg_size, limit).await,
        Layer::TcpStruct(codec) => with_codec!(codec, tcp_struct(link, msg_size, limit)),

        // The buffer is a const generic, so every value needs its own instantiation and
        // the match cannot be replaced by passing a number along.
        Layer::MpscStruct(codec, RemoteBuffer::Default) => {
            with_codec!(codec, mpsc_struct::<{ rch::DEFAULT_BUFFER }>(link, msg_size, limit, None))
        }
        Layer::MpscStruct(codec, RemoteBuffer::Large) => {
            with_codec!(codec, mpsc_struct::<{ RemoteBuffer::LARGE }>(link, msg_size, limit, None))
        }

        // Everything but the transfer channel count matches the layer above, so the two
        // differ by that alone.
        Layer::MpscStructParallel(codec, parallel) => {
            with_codec!(codec, mpsc_struct::<{ rch::DEFAULT_BUFFER }>(link, msg_size, limit, Some(parallel)))
        }
    }
}

async fn raw_tcp(link: Link, msg_size: usize, limit: Duration) -> Result<Outcome> {
    let (client, server) = connect(link).await?;
    let (_client_reader, client_writer) = (client.reader, client.writer);
    let (mut server_reader, _server_writer) = (server.reader, server.writer);

    let payload = vec![FILL; msg_size];

    let sending = tokio::spawn(async move {
        let mut client_writer = BufWriter::with_capacity(TCP_BUFFER, client_writer);
        loop {
            client_writer.write_all(&payload).await?;
        }
        #[allow(unreachable_code)]
        std::result::Result::<(), io::Error>::Ok(())
    });

    let outcome = receive_for!(limit, meter, {
        let mut buf = vec![0u8; 256 * 1024];
        let mut pending = 0;
        loop {
            match server_reader.read(&mut buf).await? {
                0 => break,
                n => pending += n,
            }

            // The socket delivers a byte stream, so messages are counted by their size.
            while pending >= msg_size {
                pending -= msg_size;
                meter.record(msg_size);
            }
        }
    });

    settle(sending).await?;
    Ok(outcome)
}

/// A raw chmux port, reached by sending one half of a [`rch::bin`] channel over the
/// base channel and unwrapping it.
async fn chmux_port(link: Link, msg_size: usize, limit: Duration) -> Result<Outcome> {
    let cfg = Cfg::default();
    let (client, server) = connect(link).await?;

    let ((mut base_tx, _), (_, mut base_rx)) = tokio::try_join!(
        endpoints::<rch::bin::Receiver, (), codec::Default>(cfg.clone(), client),
        endpoints::<(), rch::bin::Receiver, codec::Default>(cfg, server),
    )?;

    let (bin_tx, bin_rx) = rch::bin::channel();
    base_tx.send(bin_rx).await.map_err(|err| err.without_item())?;
    let bin_rx = base_rx.recv().await?.ok_or("no binary channel")?;

    let (mut tx, mut rx) = tokio::try_join!(bin_tx.into_inner(), bin_rx.into_inner())?;

    let payload = Bytes::from(vec![FILL; msg_size]);

    let sending = tokio::spawn(async move {
        loop {
            tx.send(payload.clone()).await?;
        }
        #[allow(unreachable_code)]
        std::result::Result::<(), chmux::SendError>::Ok(())
    });

    let outcome = receive_for!(limit, meter, {
        while let Some(data) = rx.recv().await? {
            meter.record(data.remaining());
        }
    });

    settle(sending).await?;
    Ok(outcome)
}

async fn base(link: Link, msg_size: usize, limit: Duration) -> Result<Outcome> {
    let cfg = Cfg::default();
    let (client, server) = connect(link).await?;

    let ((mut tx, _), (_, mut rx)) = tokio::try_join!(
        endpoints::<Bytes, (), codec::Default>(cfg.clone(), client),
        endpoints::<(), Bytes, codec::Default>(cfg, server),
    )?;

    let payload = Bytes::from(vec![FILL; msg_size]);

    let sending = tokio::spawn(async move {
        loop {
            tx.send(payload.clone()).await?;
        }
        #[allow(unreachable_code)]
        std::result::Result::<(), rch::base::SendError<Bytes>>::Ok(())
    });

    let outcome = receive_for!(limit, meter, {
        while let Some(msg) = rx.recv().await? {
            meter.record(msg.len());
        }
    });

    settle(sending).await?;
    Ok(outcome)
}

async fn mpsc(link: Link, msg_size: usize, limit: Duration) -> Result<Outcome> {
    let cfg = Cfg::default();
    let (client, server) = connect(link).await?;

    type Transferred = rch::mpsc::Receiver<Bytes>;
    let ((mut base_tx, _), (_, mut base_rx)) = tokio::try_join!(
        endpoints::<Transferred, (), codec::Default>(cfg.clone(), client),
        endpoints::<(), Transferred, codec::Default>(cfg, server),
    )?;

    let (tx, data_rx) = rch::mpsc::with_local_buffer::<Bytes, _>(MPSC_BUFFER);
    base_tx.send(data_rx).await?;
    let mut rx = base_rx.recv().await?.ok_or("connection closed")?;

    let payload = Bytes::from(vec![FILL; msg_size]);

    let sending = tokio::spawn(async move {
        loop {
            tx.send(payload.clone()).await?;
        }
        #[allow(unreachable_code)]
        std::result::Result::<(), rch::mpsc::SendError<Bytes>>::Ok(())
    });

    let outcome = receive_for!(limit, meter, {
        while let Some(msg) = rx.recv().await? {
            meter.record(msg.len());
        }
    });

    settle(sending).await?;
    Ok(outcome)
}

/// Length-prefixed batches over plain TCP, encoded and decoded with codec `C`.
async fn tcp_struct<C: codec::Codec>(link: Link, msg_size: usize, limit: Duration) -> Result<Outcome> {
    let (client, server) = connect(link).await?;
    let client_writer = client.writer;
    let mut server_reader = server.reader;

    let sample_len = Sample::encoded_len::<C>();
    let payload = Sample::batch::<C>(msg_size);

    let sending = tokio::spawn(async move {
        let mut client_writer = BufWriter::with_capacity(TCP_BUFFER, client_writer);
        let mut buf = Vec::new();
        loop {
            // Cloned although serializing `payload` directly would do, because the
            // channel layer has to hand over ownership and clones for that. Both sides
            // of the comparison thus pay for producing the batch.
            let batch = payload.clone();

            buf.clear();
            <C as codec::Codec>::serialize(&mut buf, &batch).map_err(io::Error::other)?;
            client_writer.write_u32(buf.len() as u32).await?;
            client_writer.write_all(&buf).await?;
        }
        #[allow(unreachable_code)]
        std::result::Result::<(), io::Error>::Ok(())
    });

    let outcome = receive_for!(limit, meter, {
        let mut buf = Vec::new();
        loop {
            let len = match server_reader.read_u32().await {
                Ok(len) => len as usize,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err.into()),
            };
            buf.resize(len, 0);
            server_reader.read_exact(&mut buf).await?;

            let batch: Vec<Sample> = <C as codec::Codec>::deserialize(&buf[..])?;
            meter.record(sample_len * batch.len());
        }
    });

    settle(sending).await?;
    Ok(outcome)
}

/// Batches of records over an [`rch::mpsc`] channel, encoded with codec `C`.
///
/// `BUFFER` is the item buffer the receiving side allocates when the channel half arrives
/// there; see [`RemoteBuffer`]. `parallel` is the number of additional transfer channels,
/// or [`None`] to leave it at the library default.
async fn mpsc_struct<C: codec::Codec, const BUFFER: usize>(
    link: Link, msg_size: usize, limit: Duration, parallel: Option<usize>,
) -> Result<Outcome> {
    let cfg = Cfg::default();
    let (client, server) = connect(link).await?;

    type Transferred<C, const BUFFER: usize> = rch::mpsc::Receiver<Vec<Sample>, C, BUFFER>;
    let ((mut base_tx, _), (_, mut base_rx)) = tokio::try_join!(
        endpoints::<Transferred<C, BUFFER>, (), C>(cfg.clone(), client),
        endpoints::<(), Transferred<C, BUFFER>, C>(cfg, server),
    )?;

    let channel = rch::mpsc::with_local_buffer::<Vec<Sample>, C>(MPSC_BUFFER).with_buffer::<BUFFER>();
    let (tx, data_rx) = match parallel {
        Some(parallel) => channel.with_parallel(parallel),
        None => channel,
    };

    base_tx.send(data_rx).await?;
    let mut rx = base_rx.recv().await?.ok_or("connection closed")?;

    let sample_len = Sample::encoded_len::<C>();
    let payload = Sample::batch::<C>(msg_size);

    let sending = tokio::spawn(async move {
        loop {
            tx.send(payload.clone()).await?;
        }
        #[allow(unreachable_code)]
        std::result::Result::<(), rch::mpsc::SendError<Vec<Sample>>>::Ok(())
    });

    let outcome = receive_for!(limit, meter, {
        while let Some(msg) = rx.recv().await? {
            meter.record(sample_len * msg.len());
        }
    });

    settle(sending).await?;
    Ok(outcome)
}

/// Reports a failure of the sending task, or cancels it if the transfer stopped early.
async fn settle<E>(sending: tokio::task::JoinHandle<std::result::Result<(), E>>) -> Result<()>
where
    E: Error + Send + Sync + 'static,
{
    if sending.is_finished() {
        sending.await??;
    } else {
        sending.abort();
    }

    Ok(())
}

/// Establishes a Remoc connection over one side of a link and spawns its dispatcher.
async fn endpoints<Tx, Rx, C>(
    cfg: Cfg, side: LinkSide,
) -> Result<(rch::base::Sender<Tx, C>, rch::base::Receiver<Rx, C>)>
where
    Tx: RemoteSend,
    Rx: RemoteSend,
    C: codec::Codec,
{
    let (conn, tx, rx) = Connect::io::<_, _, _, _, C>(cfg, side.reader, side.writer).await?;
    tokio::spawn(conn);
    Ok((tx, rx))
}
