use futures::StreamExt;

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel_with_cfg;
use remoc::{
    codec, exec,
    rch::{
        broadcast::{self, ReceiverStream},
        mpsc,
    },
};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let cfg = remoc::chmux::Cfg { chunk_size: 4, receive_buffer: 4, ..Default::default() };
    let ((mut a_tx, _), (_, mut b_rx)) =
        loop_channel_with_cfg::<broadcast::Receiver<(i16, mpsc::Sender<()>), codec::Default, 16>>(cfg).await;

    let (tx, rx1) = broadcast::channel::<_, _, 16>(16);
    let rx2 = tx.subscribe::<16>(16);
    let rx3 = tx.subscribe::<16>(16);

    let send_task = exec::spawn(async move {
        println!("Sending remote broadcast channel receivers");
        a_tx.send(rx1).await.unwrap();
        a_tx.send(rx2).await.unwrap();
        a_tx.send(rx3).await.unwrap();
    });

    println!("Receiving remote broadcast channel receivers");
    let mut rx1 = b_rx.recv().await.unwrap().unwrap();
    let mut rx2 = b_rx.recv().await.unwrap().unwrap();
    let mut rx3 = b_rx.recv().await.unwrap().unwrap();

    send_task.await.unwrap();

    let rx1_task = exec::spawn(async move {
        let mut i = 0;
        loop {
            match rx1.recv().await {
                Ok((msg, reply_tx)) => {
                    println!("RX1: {msg}");
                    assert_eq!(msg, i);
                    reply_tx.send(()).await.unwrap();
                    i += 1;
                }
                Err(err) if err.is_closed() => break,
                Err(err) => panic!("RX1 error: {err}"),
            }
        }
    });

    let rx2_task = exec::spawn(async move {
        let mut i = 0;
        loop {
            match rx2.recv().await {
                Ok((msg, reply_tx)) => {
                    println!("RX2: {msg}");
                    assert_eq!(msg, i);
                    reply_tx.send(()).await.unwrap();
                    i += 1;
                }
                Err(err) if err.is_closed() => break,
                Err(err) => panic!("RX2 error: {err}"),
            }
        }
    });

    let (rx3_go_tx, rx3_go_rx) = tokio::sync::oneshot::channel();
    let rx3_task = exec::spawn(async move {
        rx3_go_rx.await.unwrap();

        let mut lagged = false;
        loop {
            match rx3.recv().await {
                Ok((msg, _reply_tx)) => {
                    println!("RX3: {msg}");
                }
                Err(err) if err.is_closed() => break,
                Err(err) if err.is_lagged() => {
                    lagged = true;
                    println!("RX3 lagged");
                }
                Err(err) => panic!("RX3 error: {err}"),
            }
        }
        assert!(lagged, "RX3 did not lag behind");
    });

    let mut rx3_go_tx = Some(rx3_go_tx);
    for i in 0..128 {
        println!("Sending {i}");
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        let tx = tx.clone();
        tx.send((i, reply_tx)).unwrap();

        if i == 64 {
            rx3_go_tx.take().unwrap().send(()).unwrap();
        }

        for r in 1..=2 {
            println!("Waiting for reply {r}/2");
            reply_rx.recv().await.unwrap();
        }
    }
    drop(tx);

    println!("Waiting for tasks to finish");
    tokio::try_join!(rx1_task, rx2_task, rx3_task).unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple_stream() {
    crate::init();
    let cfg = remoc::chmux::Cfg { chunk_size: 4, receive_buffer: 4, ..Default::default() };
    let ((mut a_tx, _), (_, mut b_rx)) =
        loop_channel_with_cfg::<broadcast::Receiver<(i16, mpsc::Sender<()>), codec::Default, 16>>(cfg).await;

    let (tx, rx1) = broadcast::channel::<_, _, 16>(16);
    let rx2 = tx.subscribe::<16>(16);
    let rx3 = tx.subscribe::<16>(16);

    let send_task = exec::spawn(async move {
        println!("Sending remote broadcast channel receivers");
        a_tx.send(rx1).await.unwrap();
        a_tx.send(rx2).await.unwrap();
        a_tx.send(rx3).await.unwrap();
    });

    println!("Receiving remote broadcast channel receivers");
    let mut rx1 = ReceiverStream::from(b_rx.recv().await.unwrap().unwrap());
    let mut rx2 = ReceiverStream::from(b_rx.recv().await.unwrap().unwrap());
    let mut rx3 = ReceiverStream::from(b_rx.recv().await.unwrap().unwrap());

    send_task.await.unwrap();

    let rx1_task = exec::spawn(async move {
        let mut i = 0;
        loop {
            match rx1.next().await {
                Some(Ok((msg, reply_tx))) => {
                    println!("RX1: {msg}");
                    assert_eq!(msg, i);
                    reply_tx.send(()).await.unwrap();
                    i += 1;
                }
                None => break,
                Some(Err(err)) => panic!("RX1 error: {err}"),
            }
        }
    });

    let rx2_task = exec::spawn(async move {
        let mut i = 0;
        loop {
            match rx2.next().await {
                Some(Ok((msg, reply_tx))) => {
                    println!("RX2: {msg}");
                    assert_eq!(msg, i);
                    reply_tx.send(()).await.unwrap();
                    i += 1;
                }
                None => break,
                Some(Err(err)) => panic!("RX2 error: {err}"),
            }
        }
    });

    let (rx3_go_tx, rx3_go_rx) = tokio::sync::oneshot::channel();
    let rx3_task = exec::spawn(async move {
        rx3_go_rx.await.unwrap();

        let mut lagged = false;
        loop {
            match rx3.next().await {
                Some(Ok((msg, _reply_tx))) => {
                    println!("RX3: {msg}");
                }
                None => break,
                Some(Err(err)) if err.is_lagged() => {
                    lagged = true;
                    println!("RX3 lagged");
                }
                Some(Err(err)) => panic!("RX3 error: {err}"),
            }
        }
        assert!(lagged, "RX3 did not lag behind");
    });

    let mut rx3_go_tx = Some(rx3_go_tx);
    for i in 0..128 {
        println!("Sending {i}");
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        let tx = tx.clone();
        tx.send((i, reply_tx)).unwrap();

        if i == 64 {
            rx3_go_tx.take().unwrap().send(()).unwrap();
        }

        for r in 1..=2 {
            println!("Waiting for reply {r}/2");
            reply_rx.recv().await.unwrap();
        }
    }
    drop(tx);

    println!("Waiting for tasks to finish");
    tokio::try_join!(rx1_task, rx2_task, rx3_task).unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn weak_sender() {
    crate::init();

    let (tx, mut rx) = broadcast::channel::<i32, codec::Default, 16>(16);

    // Downgrade to a weak sender.
    let weak = tx.downgrade();
    assert_eq!(tx.strong_count(), 1);
    assert_eq!(tx.weak_count(), 1);
    assert_eq!(weak.strong_count(), 1);
    assert_eq!(weak.weak_count(), 1);

    // Upgrade succeeds while a strong sender exists.
    let tx2 = weak.upgrade().expect("upgrade should succeed while a strong sender exists");
    assert_eq!(tx.strong_count(), 2);

    // Sending through the upgraded sender reaches the receiver.
    tx2.send(1).unwrap();
    assert_eq!(rx.recv().await.unwrap(), 1);

    drop(tx2);
    assert_eq!(tx.strong_count(), 1);

    // Sending still works through the original sender.
    tx.send(2).unwrap();
    assert_eq!(rx.recv().await.unwrap(), 2);

    // After dropping all strong senders, only the weak sender remains.
    drop(tx);
    assert_eq!(weak.strong_count(), 0);

    // Upgrade now fails.
    assert!(weak.upgrade().is_none());

    // The channel is closed and the receiver observes this.
    assert!(matches!(rx.recv().await, Err(broadcast::RecvError::Closed)));
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn weak_sender_keeps_channel_open_only_while_strong_exists() {
    crate::init();

    let (tx, mut rx) = broadcast::channel::<i32, codec::Default, 16>(16);
    let weak = tx.downgrade();

    // Keep only a weak sender around and drop the strong one.
    drop(tx);

    // Channel is closed since no strong sender remains.
    assert!(weak.upgrade().is_none());
    assert_eq!(weak.strong_count(), 0);
    assert!(matches!(rx.recv().await, Err(broadcast::RecvError::Closed)));
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn weak_sender_default() {
    crate::init();

    // A default weak sender can never be upgraded.
    let weak = broadcast::WeakSender::<i32, codec::Default>::default();
    assert_eq!(weak.strong_count(), 0);
    assert!(weak.upgrade().is_none());
}
