use std::time::Duration;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    exec::time::sleep,
    robs::{
        RecvError,
        list::{ListEvent, ObservableList},
    },
};

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn standalone() {
    let mut obs: ObservableList<_, remoc::codec::Default> = ObservableList::new();

    obs.push(0);
    obs.push(10);
    obs.push(20);
    assert_eq!(obs.len(), 3);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn events() {
    let mut obs: ObservableList<_, remoc::codec::Default> = ObservableList::new();

    let mut sub = obs.subscribe();
    assert!(!sub.is_complete());
    assert!(!sub.is_done());

    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::InitialComplete));
    assert!(sub.is_complete());
    assert!(!sub.is_done());

    obs.push(0u32);
    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::Push(0)));

    obs.push(10);
    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::Push(10)));

    obs.push(20);
    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::Push(20)));

    obs.done();
    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::Done));
    assert!(sub.is_done());
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn events_incremental() {
    let hs = vec![0u32, 1, 2];
    let mut obs: ObservableList<_, remoc::codec::Default> = ObservableList::from(hs.clone());

    let mut sub = obs.subscribe();
    assert!(!sub.is_complete());
    assert!(!sub.is_done());

    let mut hs2 = Vec::new();
    for _ in 0..3 {
        match sub.recv().await.unwrap() {
            Some(ListEvent::Push(k)) => {
                hs2.push(k);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(hs, hs2);

    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::InitialComplete));
    assert!(sub.is_complete());
    assert!(!sub.is_done());

    obs.done();
    assert_eq!(sub.recv().await.unwrap(), Some(ListEvent::Done));
    assert!(sub.is_done());
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn mirrored() {
    let mut pre = Vec::new();
    for i in 1000..1500i32 {
        pre.push(i);
    }
    let len = pre.len();

    let mut obs: ObservableList<_, remoc::codec::Default> = ObservableList::from(pre);
    assert_eq!(obs.len(), len);

    let sub = obs.subscribe();
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.push(i);
    }

    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        assert!(!mb.is_done());

        println!("original: {obs:?}");
        println!("mirrored: {mb:?}");

        if *mb == *obs.borrow().await {
            assert!(mb.is_complete());
            break;
        }

        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }

    assert!(!obs.is_done());
    println!("done");
    obs.done();
    assert!(obs.is_done());
    mirror.changed().await;

    {
        let mb = mirror.borrow().await.unwrap();
        assert!(mb.is_done());
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn mirrored_disconnect() {
    let mut obs: ObservableList<_, remoc::codec::Default> = ObservableList::new();

    let sub = obs.subscribe();
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.push(i);
    }

    println!("drop");
    drop(obs);

    loop {
        mirror.changed().await;
        if let Err(RecvError::Closed) = mirror.borrow().await {
            break;
        }
    }
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn mirrored_disconnect_after_done() {
    let mut obs: ObservableList<_, remoc::codec::Default> = ObservableList::new();

    let sub = obs.subscribe();
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.push(i);
    }

    println!("done and drop");
    obs.done();

    loop {
        let mb = mirror.borrow_and_update().await.unwrap();

        println!("mirrored: {mb:?}");
        if *mb == *obs.borrow().await {
            break;
        }

        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }

    let mb = mirror.borrow_and_update().await.unwrap();
    assert!(mb.is_complete());
    assert!(mb.is_done());
}
