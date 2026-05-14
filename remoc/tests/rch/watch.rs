use futures::StreamExt;
use std::time::Duration;

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::{droppable_loop_channel, loop_channel};
use remoc::{
    exec,
    exec::time::sleep,
    rch::{
        base::SendErrorKind,
        watch::{self, ChangedError, ReceiverStream, SendError},
    },
};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let start_value = 2;
    let end_value = 124;

    println!("Sending remote mpsc channel receiver");
    let (mut tx, rx) = watch::channel(start_value);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote mpsc channel receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    {
        let value = rx.borrow().unwrap();
        println!("Initial value: {value:?}");
    }

    let recv_task = exec::spawn(async move {
        let mut value = *rx.borrow().unwrap();
        assert_eq!(value, start_value);

        while rx.changed().await.is_ok() {
            value = *rx.borrow_and_update().unwrap();
            println!("Received value change: {value}");
        }

        value = *rx.borrow_and_update().unwrap();
        assert_eq!(value, end_value);
    });

    for value in start_value..=end_value {
        println!("Sending {value}");
        tx.send(value).unwrap();
        assert_eq!(*tx.borrow(), value);

        if value % 10 == 0 {
            sleep(Duration::from_millis(20)).await;
        }
    }

    tx.check().unwrap();
    drop(tx);

    println!("Waiting for receive task");
    recv_task.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple_stream() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let start_value = 2;
    let end_value = 124;

    println!("Sending remote mpsc channel receiver");
    let (mut tx, rx) = watch::channel(start_value);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote mpsc channel receiver");
    let rx = b_rx.recv().await.unwrap().unwrap();
    let mut rx = ReceiverStream::from(rx);

    let recv_task = exec::spawn(async move {
        let mut value = 0;
        while let Some(rxed_value) = rx.next().await {
            value = rxed_value.unwrap();
            println!("Received value change: {value}");
        }

        assert_eq!(value, end_value);
    });

    let mut prev_value = start_value;
    for value in start_value..=end_value {
        println!("Sending {value}");
        let last_value = tx.send_replace(value);
        assert_eq!(last_value, prev_value);
        assert_eq!(*tx.borrow(), value);
        prev_value = value;

        if value % 10 == 0 {
            sleep(Duration::from_millis(20)).await;

            println!("Modifying");
            tx.send_modify(|v| *v -= 1);
            prev_value -= 1;
        }
    }

    tx.check().unwrap();
    drop(tx);

    println!("Waiting for receive task");
    recv_task.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn forward() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let start_value = 2;
    let end_value = 124;

    let (tx, local_rx) = tokio::sync::watch::channel(start_value);

    println!("Forwarding remote mpsc channel receiver");
    let (forward, rx) = watch::forward(local_rx);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote mpsc channel receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    {
        let value = rx.borrow().unwrap();
        println!("Initial value: {value:?}");
    }

    let recv_task = exec::spawn(async move {
        let mut value = *rx.borrow().unwrap();
        assert_eq!(value, start_value);

        while rx.changed().await.is_ok() {
            value = *rx.borrow_and_update().unwrap();
            println!("Received value change: {value}");
        }

        value = *rx.borrow_and_update().unwrap();
        assert_eq!(value, end_value);
    });

    for value in start_value..=end_value {
        println!("Sending {value}");
        tx.send(value).unwrap();
        assert_eq!(*tx.borrow(), value);

        if value % 10 == 0 {
            sleep(Duration::from_millis(20)).await;
        }
    }

    drop(tx);

    println!("Waiting for receive task");
    recv_task.await.unwrap();

    println!("Waiting for forward task");
    forward.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn forwarded() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let start_value = 2;
    let end_value = 32;

    let (tx, local_rx) = tokio::sync::watch::channel(start_value);

    println!("Forwarding local watch receiver via Receiver::forwarded");
    let rx = watch::Receiver::forwarded(local_rx);
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let recv_task = exec::spawn(async move {
        let mut value = *rx.borrow().unwrap();
        assert_eq!(value, start_value);

        while rx.changed().await.is_ok() {
            value = *rx.borrow_and_update().unwrap();
            println!("Received value change: {value}");
        }

        value = *rx.borrow_and_update().unwrap();
        assert_eq!(value, end_value);
    });

    for value in start_value..=end_value {
        tx.send(value).unwrap();
        if value % 5 == 0 {
            sleep(Duration::from_millis(10)).await;
        }
    }

    drop(tx);

    println!("Waiting for receive task");
    recv_task.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn modify_stream() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let start_value = 2;
    let end_value = 124;

    println!("Sending remote mpsc channel receiver");
    let (mut tx, rx) = watch::channel(start_value);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote mpsc channel receiver");
    let rx = b_rx.recv().await.unwrap().unwrap();
    let mut rx = ReceiverStream::from(rx);

    let recv_task = exec::spawn(async move {
        let mut value = 0;
        while let Some(rxed_value) = rx.next().await {
            value = rxed_value.unwrap();
            println!("Received value change: {value}");
        }

        assert_eq!(value, end_value);
    });

    for value in (start_value + 1)..=end_value {
        println!("Modifying {value}");
        tx.send_modify(|v| *v += 1);

        if value % 10 == 0 {
            sleep(Duration::from_millis(20)).await;
        }
    }

    tx.check().unwrap();
    drop(tx);

    println!("Waiting for receive task");
    recv_task.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn send_if_modified() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let start_value = 2;
    let end_value = 124;

    println!("Sending remote watch channel receiver");
    let (mut tx, rx) = watch::channel(start_value);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote watch channel receiver");
    let rx = b_rx.recv().await.unwrap().unwrap();
    let mut rx = ReceiverStream::from(rx);

    let recv_task = exec::spawn(async move {
        let mut last_value = start_value;
        while let Some(rxed_value) = rx.next().await {
            let value = rxed_value.unwrap();
            println!("Received value change: {value}");
            // Only even values should ever be observed, since odd updates are skipped.
            assert_eq!(value % 2, 0);
            last_value = value;
        }
        assert_eq!(last_value, end_value);
    });

    for value in (start_value + 1)..=end_value {
        // Only notify on even values; odd values are written but not notified.
        let notified = tx.send_if_modified(|v| {
            *v = value;
            value % 2 == 0
        });
        assert_eq!(notified, value % 2 == 0);
        assert_eq!(*tx.borrow(), value);

        if value % 10 == 0 {
            sleep(Duration::from_millis(20)).await;
        }
    }

    tx.check().unwrap();
    drop(tx);

    println!("Waiting for receive task");
    recv_task.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn close() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Sender<i16>>().await;

    println!("Sending remote mpsc channel sender");
    let (tx, rx) = watch::channel(123);
    a_tx.send(tx).await.unwrap();
    println!("Receiving remote mpsc channel sender");
    let mut tx = b_rx.recv().await.unwrap().unwrap();

    println!("Cloning receiver");
    let rx2 = rx.clone();

    assert!(!tx.is_closed());

    println!("Dropping first receiver");
    drop(rx);
    assert!(!tx.is_closed());

    println!("Dropping second receiver");
    drop(rx2);

    println!("Waiting for close notification");
    tx.closed().await;
    assert!(tx.is_closed());
    tx.check().unwrap();

    println!("Attempting to send");
    match tx.send(15) {
        Ok(()) => panic!("send succeeded after close"),
        Err(err) if err.is_closed() => (),
        Err(err) => panic!("wrong error after close: {err}"),
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn conn_failure() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx), conn) = droppable_loop_channel::<watch::Sender<i16>>().await;

    println!("Sending remote mpsc channel sender");
    let (tx, rx) = watch::channel(123);
    a_tx.send(tx).await.unwrap();
    println!("Receiving remote mpsc channel sender");
    let mut tx = b_rx.recv().await.unwrap().unwrap();

    println!("Cloning receiver");
    let _rx2 = rx.clone();

    assert!(!tx.is_closed());

    println!("Dropping connection");
    drop(conn);

    println!("Waiting for close notification");
    tx.closed().await;
    assert!(tx.is_closed());
    tx.check().unwrap();

    println!("Attempting to send");
    match tx.send(15) {
        Ok(()) => panic!("send succeeded after close"),
        Err(err) if err.is_closed() => (),
        Err(err) => panic!("wrong error after close: {err}"),
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn max_item_size_exceeded() {
    crate::init();
    if !remoc::exec::are_threads_available().await {
        println!("test requires threads");
        return;
    }

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<Vec<u8>>>().await;

    println!("Sending remote mpsc channel receiver");
    let (mut tx, rx) = watch::channel(Vec::new());
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote mpsc channel receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    assert_eq!(tx.max_item_size(), rx.max_item_size());
    let max_item_size = tx.max_item_size();
    println!("Maximum send and recv item size is {max_item_size}");

    {
        let value = rx.borrow().unwrap();
        println!("Initial value: {value:?}");
    }

    let recv_task = exec::spawn(async move {
        loop {
            let res = rx.changed().await;
            println!("RX changed result: {res:?}");
            if res.is_err() {
                break res;
            }

            let value = rx.borrow_and_update().unwrap().clone();
            println!("Received value change: {} elements", value.len());
        }
    });

    // Happy case: sent data size is under limit.
    // JSON encoding will result in much larger transfer size.
    let elems = max_item_size / 10;
    println!("Sending {elems} elements");
    let value = vec![100; elems];
    tx.send(value.clone()).unwrap();
    assert_eq!(*tx.borrow(), value);

    sleep(Duration::from_millis(100)).await;

    // Failure case: sent data size exceeds limits.
    let elems = max_item_size * 10;
    println!("Sending {elems} elements");
    let value = vec![100; elems];
    tx.send(value.clone()).unwrap();
    assert_eq!(*tx.borrow(), value);

    println!("Waiting for receive task");
    assert!(matches!(recv_task.await.unwrap(), Err(ChangedError::Closed)));

    // Send one more element to obtain error.
    println!("Sending one more element to obtain error");
    let res = tx.send(vec![1; 1]);
    println!("Result: {res:?}");
    assert!(matches!(res, Err(SendError::RemoteSend(SendErrorKind::MaxItemSizeExceeded))));

    // Test error clearing.
    assert!(matches!(tx.error(), Some(SendError::RemoteSend(SendErrorKind::MaxItemSizeExceeded))));
    tx.clear_error();
    assert!(tx.error().is_none());
    tx.check().unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn max_item_size_exceeded_check() {
    crate::init();
    if !remoc::exec::are_threads_available().await {
        println!("test requires threads");
        return;
    }

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<Vec<u8>>>().await;

    println!("Sending remote mpsc channel receiver");
    let (mut tx, rx) = watch::channel(Vec::new());
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote mpsc channel receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    assert_eq!(tx.max_item_size(), rx.max_item_size());
    let max_item_size = tx.max_item_size();
    println!("Maximum send and recv item size is {max_item_size}");

    {
        let value = rx.borrow().unwrap();
        println!("Initial value: {value:?}");
    }

    let recv_task = exec::spawn(async move {
        loop {
            let res = rx.changed().await;
            println!("RX changed result: {res:?}");
            if res.is_err() {
                break res;
            }

            let value = rx.borrow_and_update().unwrap().clone();
            println!("Received value change: {} elements", value.len());
        }
    });

    // Happy case: sent data size is under limit.
    // JSON encoding will result in much larger transfer size.
    let elems = max_item_size / 10;
    println!("Sending {elems} elements");
    let value = vec![100; elems];
    tx.send(value.clone()).unwrap();
    assert_eq!(*tx.borrow(), value);

    sleep(Duration::from_millis(100)).await;

    // Failure case: sent data size exceeds limits.
    let elems = max_item_size * 10;
    println!("Sending {elems} elements");
    let value = vec![100; elems];
    tx.send(value.clone()).unwrap();
    assert_eq!(*tx.borrow(), value);

    println!("Wait for sender close");
    tx.closed().await;
    let res = tx.check();
    println!("Sender check result: {res:?}");
    assert!(matches!(res, Err(SendError::RemoteSend(SendErrorKind::MaxItemSizeExceeded))));

    println!("Waiting for receive task");
    assert!(matches!(recv_task.await.unwrap(), Err(ChangedError::Closed)));
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn has_changed() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let (tx, rx) = watch::channel(10);
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // After initial borrow_and_update, value is seen — has_changed should be false.
    let value = *rx.borrow_and_update().unwrap();
    assert_eq!(value, 10);
    assert!(!rx.has_changed().unwrap());

    // Send a new value — has_changed should eventually become true.
    tx.send(20).unwrap();
    let mut n = 1000;
    while !rx.has_changed().unwrap() {
        assert!(n > 0, "timed out waiting for has_changed");
        sleep(Duration::from_millis(10)).await;
        n -= 1;
    }

    // After borrow_and_update, has_changed should be false again.
    let value = *rx.borrow_and_update().unwrap();
    assert_eq!(value, 20);
    assert!(!rx.has_changed().unwrap());

    // Drop sender — has_changed should eventually return Err(Closed).
    drop(tx);
    let mut n = 1000;
    loop {
        match rx.has_changed() {
            Err(_) => break,
            Ok(_) => {
                assert!(n > 0, "timed out waiting for closed error");
                sleep(Duration::from_millis(10)).await;
                n -= 1;
            }
        }
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mark_changed_and_unchanged() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let (tx, rx) = watch::channel(10);
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Consume the initial value.
    let _ = rx.borrow_and_update().unwrap();
    assert!(!rx.has_changed().unwrap());

    // mark_changed should make has_changed return true.
    rx.mark_changed();
    assert!(rx.has_changed().unwrap());

    // borrow_and_update should clear the changed flag.
    let _ = rx.borrow_and_update().unwrap();
    assert!(!rx.has_changed().unwrap());

    // Send a new value so has_changed becomes true, then mark_unchanged.
    tx.send(30).unwrap();
    let mut n = 1000;
    while !rx.has_changed().unwrap() {
        assert!(n >= 0, "timed out waiting for has_changed");
        sleep(Duration::from_millis(10)).await;
        n -= 1;
    }

    rx.mark_unchanged();
    assert!(!rx.has_changed().unwrap());

    // The value should still be readable even though we marked unchanged.
    let value = *rx.borrow().unwrap();
    assert_eq!(value, 30);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn wait_for_immediate() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let (_tx, rx) = watch::channel(42);
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // wait_for should return immediately if the current value satisfies the predicate.
    let value = rx.wait_for(|v| *v == 42).await.unwrap();
    assert_eq!(*value, 42);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn wait_for_future_value() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let (tx, rx) = watch::channel(0);
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Spawn a task that sends the target value after a delay.
    let send_task = exec::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        tx.send(5).unwrap();
        sleep(Duration::from_millis(50)).await;
        tx.send(10).unwrap();
        sleep(Duration::from_millis(50)).await;
        tx.send(100).unwrap();
        tx
    });

    // wait_for should block until the predicate is satisfied.
    let value = rx.wait_for(|v| *v >= 100).await.unwrap();
    assert!(*value >= 100);

    let _tx = send_task.await.unwrap();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn wait_for_closed() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    let (tx, rx) = watch::channel(0);
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Drop sender after a short delay — wait_for should return an error.
    let _drop_task = exec::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        drop(tx);
    });

    let result = rx.wait_for(|v| *v == 999).await;
    assert!(result.is_err(), "wait_for should fail when sender is dropped");
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn conn_failure_receiver_changed() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx), conn) = droppable_loop_channel::<watch::Receiver<i16>>().await;

    println!("Sending remote watch receiver");
    let (tx, rx) = watch::channel(123);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote watch receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Read initial value.
    let initial = *rx.borrow_and_update().unwrap();
    assert_eq!(initial, 123);

    // Send a value and wait for it to be observed, so the channel is fully established.
    tx.send(456).unwrap();
    let changed_res = rx.changed().await;
    println!("changed() after normal send returned: {changed_res:?}");
    assert!(changed_res.is_ok());
    assert_eq!(*rx.borrow_and_update().unwrap(), 456);

    println!("Dropping connection");
    drop(conn);

    // Drop the original sender too, so the only thing that can close the
    // remote receiver's underlying channel is loss of the connection.
    // We keep tx alive on purpose to make sure the close signal comes from
    // the broken connection, not from sender drop.
    let _keep_tx_alive = tx;

    println!("Waiting for changed() on receiver after connection drop");
    let res = rx.changed().await;
    println!("changed() result after connection drop: {res:?}");

    // After the connection is forcefully dropped, the forwarder injects a final
    // `RecvError` into the underlying watch channel. `changed()` must surface
    // this as `Err(ChangedError::RecvError(_))` rather than `Ok(())`.
    let err = match res {
        Err(ChangedError::Recv(err)) => err,
        other => panic!("expected ChangedError::RecvError, got: {other:?}"),
    };
    assert!(err.is_final(), "RecvError reported by changed() must be final");

    // `borrow()` still surfaces the underlying RecvError.
    let borrow_res = rx.borrow().map(|v| *v);
    println!("borrow() after connection drop: {borrow_res:?}");
    assert!(borrow_res.is_err());

    // Subsequent `changed()` calls keep returning the same final RecvError
    // (idempotent terminal state) rather than degrading to `Closed`.
    let res2 = rx.changed().await;
    println!("Second changed() result: {res2:?}");
    assert!(matches!(res2, Err(ChangedError::Recv(ref e)) if e.is_final()));

    // `has_changed()` also surfaces the final RecvError.
    assert!(matches!(rx.has_changed(), Err(ChangedError::Recv(ref e)) if e.is_final()));
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn sender_drop_receiver_changed() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<watch::Receiver<i16>>().await;

    println!("Sending remote watch receiver");
    let (tx, rx) = watch::channel(1);
    a_tx.send(rx).await.unwrap();
    println!("Receiving remote watch receiver");
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Establish channel with a regular value change.
    assert_eq!(*rx.borrow_and_update().unwrap(), 1);
    tx.send(2).unwrap();
    let res = rx.changed().await;
    println!("changed() after normal send returned: {res:?}");
    assert!(res.is_ok());
    assert_eq!(*rx.borrow_and_update().unwrap(), 2);

    println!("Dropping remote sender (clean close)");
    drop(tx);

    // A clean sender drop must be reported as Closed, not as a RecvError.
    println!("Waiting for changed() after sender drop");
    let res = rx.changed().await;
    println!("changed() result after sender drop: {res:?}");
    assert!(matches!(res, Err(ChangedError::Closed)));

    // Idempotent: repeated calls keep returning Closed.
    let res2 = rx.changed().await;
    println!("Second changed() result: {res2:?}");
    assert!(matches!(res2, Err(ChangedError::Closed)));

    // `has_changed()` reports closure too.
    assert!(matches!(rx.has_changed(), Err(ChangedError::Closed)));

    // The last value before close is still observable via `borrow()`.
    assert_eq!(*rx.borrow().unwrap(), 2);
}
