//! Verification that pipelining actually saves round trips.
//!
//! The endpoints are connected through a link with an artificial latency and the
//! tests run on a paused Tokio clock, so that no wall clock time is involved and
//! the measured durations are exact. The number of round trips a series of calls
//! takes is thus obtained by dividing the elapsed time by the round trip time.
//!
//! The calls are measured on a freshly connected client, so these tests also verify
//! that using a client for the first time does not cost an additional round trip.

use std::io;

use bytes::Bytes;
use futures::{SinkExt, StreamExt, channel::mpsc, future::join};
use tokio::time::{Duration, Instant, sleep_until};

use remoc::{RemoteSend, prelude::*, rch::base, rtc::CallError};

use crate::rtc::{
    pipelined::{
        Counter, CounterClient, Directory, DirectoryClient, DirectoryObj, DirectoryServerShared, WorkError,
    },
    pipelined_chain::{Leaf, LeafClient, Middle, MiddleClient, Root, RootClient, RootServerShared, root_obj},
};

/// Latency of the simulated link in one direction.
const LATENCY: Duration = Duration::from_millis(50);

/// Time a request takes to reach the other endpoint and its response to come back.
const ROUND_TRIP: Duration = Duration::from_millis(100);

/// Forwards frames from `rx` to `tx`, delaying each of them by [`LATENCY`].
///
/// Frames are taken from `rx` as soon as they become available and the instant of
/// their arrival is calculated then, so that frames sent at the same time also
/// arrive at the same time. Sleeping before taking the next frame would instead
/// turn the latency into a bandwidth limit.
async fn delayed_link(mut rx: mpsc::Receiver<Bytes>, mut tx: mpsc::Sender<Bytes>) {
    let (queued_tx, mut queued_rx) = mpsc::unbounded::<(Instant, Bytes)>();

    let receive = async move {
        while let Some(frame) = rx.next().await {
            if queued_tx.unbounded_send((Instant::now() + LATENCY, frame)).is_err() {
                break;
            }
        }
    };

    let send = async move {
        while let Some((arrival, frame)) = queued_rx.next().await {
            sleep_until(arrival).await;
            if tx.send(frame).await.is_err() {
                break;
            }
        }
    };

    join(receive, send).await;
}

/// A pair of connected endpoints, with [`LATENCY`] between them in each direction.
async fn delayed_loop_channel<T>() -> ((base::Sender<T>, base::Receiver<T>), (base::Sender<T>, base::Receiver<T>))
where
    T: RemoteSend,
{
    let (a_tx, a_out) = mpsc::channel::<Bytes>(16);
    let (b_in, b_rx) = mpsc::channel::<Bytes>(16);
    wokio::spawn(delayed_link(a_out, b_in));

    let (b_tx, b_out) = mpsc::channel::<Bytes>(16);
    let (a_in, a_rx) = mpsc::channel::<Bytes>(16);
    wokio::spawn(delayed_link(b_out, a_in));

    let a_rx = a_rx.map(Ok::<_, io::Error>);
    let b_rx = b_rx.map(Ok::<_, io::Error>);

    let cfg = remoc::chmux::Cfg::default();

    let a = async {
        let (conn, tx, rx) = remoc::Connect::framed(cfg.clone(), a_tx, a_rx).await.unwrap();
        wokio::spawn(async move {
            let _ = conn.await;
        });
        (tx, rx)
    };

    let b = async {
        let (conn, tx, rx) = remoc::Connect::framed(cfg.clone(), b_tx, b_rx).await.unwrap();
        wokio::spawn(async move {
            let _ = conn.await;
        });
        (tx, rx)
    };

    join(a, b).await
}

/// Connects to a directory served on the other endpoint over the delayed link.
async fn directory_client() -> DirectoryClient {
    let ((mut a_tx, _), (_, mut b_rx)) = delayed_loop_channel::<DirectoryClient>().await;

    let (server, client) = DirectoryServerShared::new(std::sync::Arc::new(DirectoryObj));
    wokio::spawn(server.serve());
    a_tx.send(client).await.unwrap();

    b_rx.recv().await.unwrap().unwrap()
}

/// The number of round trips the calls performed by `work` take.
async fn round_trips(work: impl AsyncFnOnce(&DirectoryClient)) -> u32 {
    let dir = directory_client().await;

    let start = Instant::now();
    work(&dir).await;
    let elapsed = start.elapsed();

    assert_eq!(
        elapsed.as_millis() % ROUND_TRIP.as_millis(),
        0,
        "the calls took {elapsed:?}, which is not a whole number of round trips"
    );
    (elapsed.as_millis() / ROUND_TRIP.as_millis()) as u32
}

/// Opening the counter and using it takes one round trip per call without pipelining.
#[tokio::test(start_paused = true)]
async fn without_pipelining_every_call_takes_a_round_trip() {
    crate::init();

    let trips = round_trips(async |dir| {
        let mut counter = dir.open_counter("allowed".to_string()).await.unwrap();
        counter.increase(20).await.unwrap();
        assert_eq!(counter.value().await.unwrap(), 20);
    })
    .await;

    assert_eq!(trips, 3, "opening the counter and two calls on it must take three round trips");
}

/// The same calls take a single round trip when the counter is opened pipelined.
#[tokio::test(start_paused = true)]
async fn pipelining_takes_a_single_round_trip() {
    crate::init();

    let trips = round_trips(async |dir| {
        let (mut counter, counter_rx) = CounterClient::new();

        let value = async {
            Ok::<_, WorkError>(rtc::calls!(
                dir.open_counter_pipelined("allowed".to_string(), counter_rx);
                counter.increase_call(20);
                counter.value_call()
            ))
        }
        .await
        .unwrap();

        assert_eq!(value, 20);
    })
    .await;

    assert_eq!(trips, 1, "opening the counter and using it must take a single round trip");
}

/// Connects to a root object served on the other endpoint over the delayed link.
async fn root_client() -> RootClient {
    let ((mut a_tx, _), (_, mut b_rx)) = delayed_loop_channel::<RootClient>().await;

    let (server, client) = RootServerShared::new(std::sync::Arc::new(root_obj()));
    wokio::spawn(server.serve());
    a_tx.send(client).await.unwrap();

    b_rx.recv().await.unwrap().unwrap()
}

/// A chain of objects is reached within a single round trip as well.
#[tokio::test(start_paused = true)]
async fn a_chain_of_objects_takes_a_single_round_trip() {
    crate::init();

    let root = root_client().await;

    let start = Instant::now();

    let (mut middle, middle_rx) = MiddleClient::new();
    let (mut leaf, leaf_rx) = LeafClient::new();
    let value = async {
        Ok::<_, CallError>(rtc::calls!(
            root.open_middle_pipelined(middle_rx);
            middle.set_call(5);
            middle.open_leaf_pipelined(leaf_rx);
            leaf.increase_call(3);
            leaf.value_call()
        ))
    }
    .await
    .unwrap();

    let elapsed = start.elapsed();
    assert_eq!(value, 8);
    assert_eq!(elapsed, ROUND_TRIP, "reaching the leaf object must take a single round trip");
}
