#[cfg(feature = "rch")]
use futures::StreamExt;

#[allow(unused_imports)]
use std::{net::Ipv4Addr, sync::Once};

#[cfg(feature = "rch")]
#[cfg(not(target_family = "wasm"))]
use tokio::net::{TcpListener, TcpStream};

#[cfg(feature = "rch")]
use remoc::{RemoteSend, rch::base};

mod chmux;

#[cfg(feature = "serde")]
mod codec;

#[cfg(feature = "rch")]
mod connect;

#[cfg(feature = "rch")]
mod rch;

#[cfg(feature = "rfn")]
mod rfn;

#[cfg(feature = "robj")]
mod robj;

#[cfg(feature = "robs")]
mod robs;

#[cfg(feature = "rtc")]
mod rtc;

mod versioned;

static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| {
        use tracing_subscriber::{filter::EnvFilter, fmt::format::FmtSpan};
        tracing_subscriber::fmt::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_span_events(FmtSpan::NEW)
            .init();
    });
}

#[macro_export]
macro_rules! loop_transport {
    ($queue_length:expr_2021, $a_tx:ident, $a_rx:ident, $b_tx:ident, $b_rx:ident) => {
        let ($a_tx, $b_rx) = futures::channel::mpsc::channel::<bytes::Bytes>($queue_length);
        let ($b_tx, $a_rx) = futures::channel::mpsc::channel::<bytes::Bytes>($queue_length);

        let $a_rx = $a_rx.map(Ok::<_, std::io::Error>);
        let $b_rx = $b_rx.map(Ok::<_, std::io::Error>);
    };
}

#[cfg(feature = "rch")]
pub async fn loop_channel<T>() -> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>))
where
    T: RemoteSend,
{
    let cfg = remoc::chmux::Cfg::default();
    loop_channel_with_cfg(cfg).await
}

#[cfg(feature = "rch")]
pub async fn droppable_loop_channel<T>()
-> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>), tokio::sync::mpsc::Receiver<()>)
where
    T: RemoteSend,
{
    let cfg = remoc::chmux::Cfg::default();
    droppable_loop_channel_with_cfg(cfg).await
}

#[cfg(feature = "rch")]
pub async fn loop_channel_with_cfg<T>(
    cfg: remoc::chmux::Cfg,
) -> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>))
where
    T: RemoteSend,
{
    let (a, b, mut drop_rx) = droppable_loop_channel_with_cfg(cfg).await;
    wokio::spawn(async move {
        let _ = drop_rx.recv().await;
    });
    (a, b)
}

#[cfg(feature = "rch")]
pub async fn droppable_loop_channel_with_cfg<T>(
    cfg: remoc::chmux::Cfg,
) -> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>), tokio::sync::mpsc::Receiver<()>)
where
    T: RemoteSend,
{
    let (drop_tx, drop_rx) = tokio::sync::mpsc::channel(1);
    loop_transport!(0, transport_a_tx, transport_a_rx, transport_b_tx, transport_b_rx);

    let a_cfg = cfg.clone();
    let a_drop_tx = drop_tx.clone();
    let a = async move {
        let (conn, tx, rx) = remoc::Connect::framed(a_cfg, transport_a_tx, transport_a_rx).await.unwrap();
        wokio::spawn(async move {
            tokio::select! {
                biased;
                _ = a_drop_tx.closed() => (),
                _ = conn => (),
            }
        });
        (tx, rx)
    };

    let b_cfg = cfg.clone();
    let b = async move {
        let (conn, tx, rx) = remoc::Connect::framed(b_cfg, transport_b_tx, transport_b_rx).await.unwrap();
        wokio::spawn(async move {
            tokio::select! {
                biased;
                _ = drop_tx.closed() => (),
                _ = conn => (),
            }
        });
        (tx, rx)
    };

    let (a, b) = tokio::join!(a, b);
    (a, b, drop_rx)
}

#[cfg(feature = "rch")]
#[cfg(not(target_family = "wasm"))]
pub async fn tcp_loop_channel<T>(
    tcp_port: u16,
) -> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>))
where
    T: RemoteSend,
{
    let server = async move {
        let listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), tcp_port)).await.unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (socket_rx, socket_tx) = socket.into_split();
        let cfg = remoc::Cfg { io_buffer_size: 100_000, ..Default::default() };
        let (conn, tx, rx) = remoc::Connect::io(cfg, socket_rx, socket_tx).await.unwrap();
        wokio::spawn(conn);
        (tx, rx)
    };

    let client = async move {
        let socket = TcpStream::connect((Ipv4Addr::new(127, 0, 0, 1), tcp_port)).await.unwrap();
        let (socket_rx, socket_tx) = socket.into_split();
        let cfg = remoc::Cfg { io_buffer_size: 8721, ..Default::default() };
        let (conn, tx, rx) = remoc::Connect::io(cfg, socket_rx, socket_tx).await.unwrap();
        wokio::spawn(conn);
        (tx, rx)
    };

    tokio::join!(server, client)
}
