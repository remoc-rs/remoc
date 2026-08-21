//! Distribution of an mpsc channel over multiple subscribed receivers.

use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::rch::mpsc;

/// Time a distributor is given to reach an expected state before the test fails.
const LIMIT: Duration = Duration::from_secs(2);

/// Sends `item`, failing the test if the distributor does not take it.
async fn send(tx: &mpsc::Sender<u32>, item: u32) {
    timeout(LIMIT, tx.send(item)).await.expect("distributor stopped accepting items").unwrap();
}

/// Receives from `rx` until it is closed.
async fn drain(rx: &mut mpsc::Receiver<u32>) -> Vec<u32> {
    let mut items = Vec::new();
    while let Some(item) = timeout(LIMIT, rx.recv()).await.expect("receiver was not closed").unwrap() {
        items.push(item);
    }
    items
}

/// Every item reaches exactly one subscriber and none is lost.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn items_are_distributed_over_subscribers() {
    crate::init();

    let (tx, rx) = mpsc::channel(1);
    let distributor = rx.distribute(false);
    let (mut first, _first_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();
    let (mut second, _second_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();

    wokio::spawn(async move {
        for item in 1..=20 {
            send(&tx, item).await;
        }
    });

    let (mut received, second) = tokio::join!(drain(&mut first), drain(&mut second));
    received.extend(second);
    received.sort_unstable();

    assert_eq!(received, (1..=20).collect::<Vec<_>>(), "items were lost or duplicated");
}

/// A subscriber removed through its handle is closed and receives nothing further.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn removed_subscriber_is_closed() {
    crate::init();

    let (tx, rx) = mpsc::channel(1);
    let distributor = rx.distribute(true);
    let (mut removed, removed_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();
    let (mut kept, _kept_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();

    removed_handle.remove();

    // The distributor reserves a slot on a subscriber before an item is available,
    // thus one item may still be handed to the removed subscriber.
    send(&tx, 1).await;
    let removed_items = drain(&mut removed).await;
    assert!(removed_items.len() <= 1, "a removed subscriber kept receiving items");

    // The remaining items must be sent concurrently, since only a drained
    // subscriber frees capacity for the next one.
    let sender = wokio::spawn(async move {
        for item in 2..=6 {
            send(&tx, item).await;
        }
    });

    let mut received = drain(&mut kept).await;
    sender.await.unwrap();
    received.extend(removed_items);
    received.sort_unstable();

    assert_eq!(received, (1..=6).collect::<Vec<_>>(), "items were lost after a subscriber was removed");
}

/// With `wait_on_empty` the distributor survives having no subscribers at all.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn waiting_distributor_accepts_a_later_subscriber() {
    crate::init();

    let (tx, rx) = mpsc::channel(1);
    let distributor = rx.distribute(true);

    let (mut first, first_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();
    first_handle.remove();
    send(&tx, 1).await;
    let first_items = drain(&mut first).await;

    // No subscriber is left, yet the distributor keeps running.
    let (mut second, _second_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("distributor stopped").expect("distributor ended");

    let sender = wokio::spawn(async move {
        for item in 2..=5 {
            send(&tx, item).await;
        }
    });

    let mut received = drain(&mut second).await;
    sender.await.unwrap();
    received.extend(first_items);
    received.sort_unstable();

    assert_eq!(received, (1..=5).collect::<Vec<_>>(), "items were lost while no subscriber was present");
}

/// Without `wait_on_empty` the distributor ends once its last subscriber is gone.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn distributor_ends_without_subscribers() {
    crate::init();

    let (tx, rx) = mpsc::channel(1);
    let distributor = rx.distribute(false);

    let (mut only, only_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();
    only_handle.remove();
    send(&tx, 1).await;
    drain(&mut only).await;

    timeout(LIMIT, distributor.closed()).await.expect("distributor kept running without subscribers");
    let resubscribe =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing to an ended distributor hung");
    assert!(resubscribe.is_none(), "an ended distributor accepted a subscriber");
}

/// Dropping the distributor closes every subscriber.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn dropping_distributor_closes_subscribers() {
    crate::init();

    let (_tx, rx) = mpsc::channel(1);
    let distributor = rx.distribute(true);
    let (mut first, _first_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();
    let (mut second, _second_handle) =
        timeout(LIMIT, distributor.subscribe()).await.expect("subscribing hung").unwrap();

    drop(distributor);

    assert!(
        drain(&mut first).await.is_empty(),
        "a subscriber received an item after the distributor was dropped"
    );
    assert!(
        drain(&mut second).await.is_empty(),
        "a subscriber received an item after the distributor was dropped"
    );
}
