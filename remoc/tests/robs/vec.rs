use std::time::Duration;

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    exec::time::sleep,
    robs::{
        RecvError,
        vec::{ObservableVec, VecEvent},
    },
};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn standalone() {
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::new();

    obs.push(0);
    obs.push(10);
    obs.push(20);
    assert_eq!(obs.pop(), Some(20));
    assert_eq!(obs.pop(), Some(10));

    obs.remove(0);
    assert_eq!(obs.len(), 0);

    obs.clear();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn events() {
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::new();

    let mut sub = obs.subscribe(1024);
    assert!(!sub.is_complete());
    assert!(!sub.is_done());

    assert_eq!(sub.take_initial(), Some(Vec::new()));
    assert!(sub.is_complete());
    assert!(!sub.is_done());

    obs.push(0u32);
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Push(0)));

    obs.push(10);
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Push(10)));

    obs.push(20);
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Push(20)));

    obs.remove(0);
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Remove(0)));

    obs.swap_remove(0);
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::SwapRemove(0)));

    obs.clear();
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Clear));

    obs.done();
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Done));
    assert!(sub.is_done());
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn events_incremental() {
    let hs = vec![0u32, 1, 2];
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::from(hs.clone());

    let mut sub = obs.subscribe_incremental(1024);
    assert!(!sub.is_complete());
    assert!(!sub.is_done());

    let mut hs2 = Vec::new();
    for _ in 0..3 {
        match sub.recv().await.unwrap() {
            Some(VecEvent::Push(k)) => {
                hs2.push(k);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(hs, hs2);

    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::InitialComplete));
    assert!(sub.is_complete());
    assert!(!sub.is_done());

    obs.done();
    assert_eq!(sub.recv().await.unwrap(), Some(VecEvent::Done));
    assert!(sub.is_done());
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored() {
    let mut pre = Vec::new();
    for i in 1000..1500i32 {
        pre.push(i);
    }
    let len = pre.len();

    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::from(pre);
    assert_eq!(obs.len(), len);

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.insert(5, i);
    }

    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        assert!(mb.is_complete());
        assert!(!mb.is_done());

        println!("original: {obs:?}");
        println!("mirrored: {mb:?}");
        if *mb == *obs {
            break;
        }

        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }

    println!("remove");
    obs.remove(100);
    mirror.changed().await;
    obs.shrink_to_fit();
    mirror.changed().await;

    println!("set");
    *obs.get_mut(100).unwrap() = 102;
    mirror.changed().await;

    {
        let mb = mirror.borrow_and_update().await.unwrap();
        assert_eq!(*mb, *obs);
    }

    println!("clear");
    obs.clear();
    mirror.changed().await;

    {
        let mb = mirror.borrow().await.unwrap();
        assert!(mb.is_empty());
        assert!(!mb.is_done());
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

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored_disconnect() {
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::new();

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.insert(0, i);
    }

    println!("drop");
    drop(obs);

    loop {
        mirror.changed().await;
        if let Err(RecvError::Closed) = mirror.borrow().await {
            break;
        }
    }
    assert!(matches!(mirror.borrow().await, Err(RecvError::Closed)));
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored_disconnect_after_done() {
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::new();

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.push(i);
    }

    println!("done and drop");
    obs.done();
    let hs = obs.into_inner();

    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        assert!(mb.is_complete());

        println!("original: {hs:?}");
        println!("mirrored: {mb:?}");
        if *mb == hs {
            break;
        }

        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }

    let mb = mirror.borrow_and_update().await.unwrap();
    assert!(mb.is_done());
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn incremental() {
    let mut pre = Vec::new();
    for i in 0..5000 {
        pre.push(i);
    }
    let len = pre.len();

    let obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::from(pre);
    assert_eq!(obs.len(), len);

    let sub = obs.subscribe_incremental(1024);
    let mut mirror = sub.mirror(10000);

    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        #[allow(clippy::comparison_chain)]
        if mb.len() < len {
            assert!(!mb.is_complete());
        } else if mb.len() > len {
            assert!(mb.is_complete());
        }

        println!("original: {obs:?}");
        println!("mirrored: {mb:?}");
        if *mb == *obs {
            break;
        }

        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored_subscribe() {
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::new();
    for i in 0..100 {
        obs.push(i);
    }

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(10000);

    // Wait until the mirror is in sync with the observed vec.
    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        if *mb == *obs {
            break;
        }
        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }

    // Subscribe to the mirror itself.
    let mut sub2 = mirror.subscribe(1024).await.unwrap();
    assert!(!sub2.is_incremental());
    assert!(!sub2.is_done());

    let initial = sub2.take_initial().unwrap();
    assert_eq!(initial, *obs);
    assert!(sub2.is_complete());
    assert!(!sub2.is_done());

    // A change to the observed vec must propagate through the mirror to the subscription.
    obs.push(1000);
    assert_eq!(sub2.recv().await.unwrap(), Some(VecEvent::Push(1000)));

    obs.remove(0);
    assert_eq!(sub2.recv().await.unwrap(), Some(VecEvent::Remove(0)));

    // When the original observed vec ends, the subscription to the mirror must end as well.
    obs.done();
    assert_eq!(sub2.recv().await.unwrap(), Some(VecEvent::Done));
    assert!(sub2.is_done());
    assert_eq!(sub2.recv().await.unwrap(), None);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored_subscribe_incremental() {
    let mut obs: ObservableVec<_, remoc::codec::Default> = ObservableVec::new();
    for i in 0..100 {
        obs.push(i);
    }

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(10000);

    // Wait until the mirror is in sync with the observed vec.
    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        if *mb == *obs {
            break;
        }
        drop(mb);
        sleep(Duration::from_millis(100)).await;
    }

    // Subscribe to the mirror itself with incremental transfer of the current contents.
    let mut sub2 = mirror.subscribe_incremental(1024).await.unwrap();
    assert!(sub2.is_incremental());
    assert!(!sub2.is_done());

    // Receive the current contents incrementally.
    let mut v2 = Vec::new();
    loop {
        match sub2.recv().await.unwrap() {
            Some(VecEvent::Push(k)) => {
                v2.push(k);
            }
            Some(VecEvent::InitialComplete) => break,
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(v2, *obs);
    assert!(sub2.is_complete());
    assert!(!sub2.is_done());

    // A change to the observed vec must propagate through the mirror to the subscription.
    obs.push(1000);
    assert_eq!(sub2.recv().await.unwrap(), Some(VecEvent::Push(1000)));

    obs.remove(0);
    assert_eq!(sub2.recv().await.unwrap(), Some(VecEvent::Remove(0)));

    // When the original observed vec ends, the subscription to the mirror must end as well.
    obs.done();
    assert_eq!(sub2.recv().await.unwrap(), Some(VecEvent::Done));
    assert!(sub2.is_done());
    assert_eq!(sub2.recv().await.unwrap(), None);
}
