// Compile-time benchmark: remoc channels with many different types.
//
// This benchmark creates channels AND sends channel endpoints through other channels,
// forcing the compiler to monomorphize both channel creation and the
// Serialize/Deserialize impls (which are the real monomorphization hotspot).

mod types;

use futures::StreamExt;
use types::*;

use remoc::{
    RemoteSend,
    exec,
    rch::{base, mpsc, oneshot, watch},
};

/// Set up a loopback remoc connection using in-memory futures channels.
/// Returns two pairs of (Sender, Receiver) for bidirectional communication.
async fn loop_channel<T>() -> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>))
where
    T: RemoteSend,
{
    let (a_tx, b_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(0);
    let (b_tx, a_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(0);

    let a_rx = a_rx.map(Ok::<_, std::io::Error>);
    let b_rx = b_rx.map(Ok::<_, std::io::Error>);

    let cfg = remoc::chmux::Cfg::default();

    let a_cfg = cfg.clone();
    let a = async move {
        let (conn, tx, rx) = remoc::Connect::framed(a_cfg, a_tx, a_rx).await.unwrap();
        exec::spawn(conn);
        (tx, rx)
    };

    let b_cfg = cfg.clone();
    let b = async move {
        let (conn, tx, rx) = remoc::Connect::framed(b_cfg, b_tx, b_rx).await.unwrap();
        exec::spawn(conn);
        (tx, rx)
    };

    tokio::join!(a, b)
}

/// Send an mpsc::Sender<T> through a remoc connection.
/// This forces monomorphization of Serialize for mpsc::Sender<T>
/// and Deserialize for mpsc::Sender<T>.
async fn send_mpsc_sender<T: RemoteSend>() {
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<mpsc::Sender<T>>().await;

    let (tx, _rx) = mpsc::channel::<T, remoc::codec::Default>(16);
    a_tx.send(tx).await.unwrap();
    let _remote_tx = b_rx.recv().await.unwrap().unwrap();
}

/// Send an mpsc::Receiver<T> through a remoc connection.
async fn send_mpsc_receiver<T: RemoteSend>() {
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<mpsc::Receiver<T>>().await;

    let (_tx, rx) = mpsc::channel::<T, remoc::codec::Default>(16);
    a_tx.send(rx).await.unwrap();
    let _remote_rx = b_rx.recv().await.unwrap().unwrap();
}

/// Send a watch::Receiver<T> through a remoc connection.
async fn send_watch_receiver<T: RemoteSend + Sync + Clone>(val: T) {
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<watch::Receiver<T>>().await;

    let (_tx, rx) = watch::channel::<T, remoc::codec::Default>(val);
    a_tx.send(rx).await.unwrap();
    let _remote_rx = b_rx.recv().await.unwrap().unwrap();
}

/// Send an oneshot channel pair through a remoc connection.
async fn send_oneshot<T: RemoteSend>() {
    let ((mut a_tx, _a_rx), (_b_tx, mut b_rx)) = loop_channel::<oneshot::Sender<T>>().await;

    let (tx, _rx) = oneshot::channel::<T, remoc::codec::Default>();
    a_tx.send(tx).await.unwrap();
    let _remote_tx = b_rx.recv().await.unwrap().unwrap();
}

#[tokio::main]
async fn main() {
    // Send mpsc senders through a connection for each type.
    // This monomorphizes Serialize/Deserialize for mpsc::Sender<T>.
    send_mpsc_sender::<Simple>().await;
    send_mpsc_sender::<Color>().await;
    send_mpsc_sender::<Address>().await;
    send_mpsc_sender::<Person>().await;
    send_mpsc_sender::<Company>().await;
    send_mpsc_sender::<Message>().await;
    send_mpsc_sender::<Config>().await;
    send_mpsc_sender::<ConfigEntry>().await;
    send_mpsc_sender::<ConfigValue>().await;
    send_mpsc_sender::<Matrix>().await;
    send_mpsc_sender::<TimeSeries>().await;
    send_mpsc_sender::<Document>().await;
    send_mpsc_sender::<Section>().await;
    send_mpsc_sender::<Figure>().await;
    send_mpsc_sender::<Event>().await;
    send_mpsc_sender::<EventKind>().await;
    send_mpsc_sender::<ApiResponse>().await;
    send_mpsc_sender::<ResponseBody>().await;
    send_mpsc_sender::<DatabaseRecord>().await;
    send_mpsc_sender::<Workspace>().await;

    // Send mpsc receivers through a connection for each type.
    send_mpsc_receiver::<Simple>().await;
    send_mpsc_receiver::<Color>().await;
    send_mpsc_receiver::<Address>().await;
    send_mpsc_receiver::<Person>().await;
    send_mpsc_receiver::<Company>().await;
    send_mpsc_receiver::<Message>().await;
    send_mpsc_receiver::<Config>().await;
    send_mpsc_receiver::<ConfigEntry>().await;
    send_mpsc_receiver::<ConfigValue>().await;
    send_mpsc_receiver::<Matrix>().await;
    send_mpsc_receiver::<TimeSeries>().await;
    send_mpsc_receiver::<Document>().await;
    send_mpsc_receiver::<Section>().await;
    send_mpsc_receiver::<Figure>().await;
    send_mpsc_receiver::<Event>().await;
    send_mpsc_receiver::<EventKind>().await;
    send_mpsc_receiver::<ApiResponse>().await;
    send_mpsc_receiver::<ResponseBody>().await;
    send_mpsc_receiver::<DatabaseRecord>().await;
    send_mpsc_receiver::<Workspace>().await;

    // Send watch receivers through a connection for a subset of types.
    send_watch_receiver(Simple {
        a: 0, b: 0, c: 0, d: 0, e: 0, f: 0, g: 0, h: 0,
        i: 0.0, j: 0.0, k: false, l: String::new(), m: Vec::new(),
    }).await;
    send_watch_receiver(Color::Red).await;
    send_watch_receiver(Address {
        street: String::new(), city: String::new(), state: String::new(),
        zip: String::new(), country: String::new(),
    }).await;
    send_watch_receiver(0u32).await;
    send_watch_receiver(String::new()).await;
    send_watch_receiver(Vec::<u8>::new()).await;
    send_watch_receiver(Matrix { rows: 0, cols: 0, data: Vec::new() }).await;
    send_watch_receiver(ConfigValue::Bool(false)).await;
    send_watch_receiver(Figure { caption: String::new(), data: Vec::new(), width: 0, height: 0 }).await;

    // Send oneshot senders through a connection for each type.
    send_oneshot::<Simple>().await;
    send_oneshot::<Color>().await;
    send_oneshot::<Address>().await;
    send_oneshot::<Person>().await;
    send_oneshot::<Company>().await;
    send_oneshot::<Message>().await;
    send_oneshot::<Config>().await;
    send_oneshot::<ConfigEntry>().await;
    send_oneshot::<ConfigValue>().await;
    send_oneshot::<Matrix>().await;
    send_oneshot::<TimeSeries>().await;
    send_oneshot::<Document>().await;
    send_oneshot::<Section>().await;
    send_oneshot::<Figure>().await;
    send_oneshot::<Event>().await;
    send_oneshot::<EventKind>().await;
    send_oneshot::<ApiResponse>().await;
    send_oneshot::<ResponseBody>().await;
    send_oneshot::<DatabaseRecord>().await;
    send_oneshot::<Workspace>().await;

    println!("remoc channels compile-time benchmark OK");
}
