use std::{collections::HashMap, time::Duration};

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    exec::time::sleep,
    robs::{
        RecvError,
        hash_map::{HashMapEvent, ObservableHashMap},
    },
};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn standalone() {
    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::new();

    obs.insert(10, "10".to_string());
    obs.insert(20, "20".to_string());
    assert_eq!(obs.get(&10), Some(&"10".to_string()));
    assert_eq!(obs.get(&20), Some(&"20".to_string()));

    obs.remove(&10);
    assert_eq!(obs.get(&10), None);

    obs.clear();
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn events() {
    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::new();

    let mut sub = obs.subscribe(1024);
    assert!(!sub.is_complete());
    assert!(!sub.is_done());

    assert_eq!(sub.take_initial(), Some(HashMap::new()));
    assert!(sub.is_complete());
    assert!(!sub.is_done());

    obs.insert(10u32, "10".to_string());
    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::Set(10, "10".to_string())));

    obs.insert(20, "20".to_string());
    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::Set(20, "20".to_string())));

    obs.remove(&10);
    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::Remove(10)));

    obs.clear();
    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::Clear));

    obs.done();
    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::Done));
    assert!(sub.is_done());
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn events_incremental() {
    let mut hm = HashMap::new();
    hm.insert(0, "zero".to_string());
    hm.insert(1, "one".to_string());
    hm.insert(2, "two".to_string());

    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::from(hm.clone());

    let mut sub = obs.subscribe_incremental(1024);
    assert!(!sub.is_complete());
    assert!(!sub.is_done());

    let mut hm2 = HashMap::new();
    for _ in 0..3 {
        match sub.recv().await.unwrap() {
            Some(HashMapEvent::Set(k, v)) => {
                hm2.insert(k, v);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(hm, hm2);

    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::InitialComplete));
    assert!(sub.is_complete());
    assert!(!sub.is_done());

    obs.done();
    assert_eq!(sub.recv().await.unwrap(), Some(HashMapEvent::Done));
    assert!(sub.is_done());
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored() {
    let mut pre = HashMap::new();
    for i in 1000..1500 {
        pre.insert(i, format!("pre {i}"));
    }
    let len = pre.len();

    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::from(pre);
    assert_eq!(obs.len(), len);

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.insert(i, format!("{i}"));
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
    obs.remove(&10);
    mirror.changed().await;
    obs.shrink_to_fit();
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
    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::new();

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.insert(i, format!("{i}"));
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
    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::new();

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(1000);

    for i in 1..500 {
        obs.insert(i, format!("{i}"));
    }

    println!("done and drop");
    obs.done();
    let hm = obs.into_inner();

    loop {
        let mb = mirror.borrow_and_update().await.unwrap();
        assert!(mb.is_complete());

        println!("original: {hm:?}");
        println!("mirrored: {mb:?}");
        if *mb == hm {
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
    let mut pre = HashMap::new();
    for i in 0..5000 {
        pre.insert(i, format!("pre {i}"));
    }
    let len = pre.len();

    let obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::from(pre);
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
    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::new();
    for i in 0..100 {
        obs.insert(i, format!("{i}"));
    }

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(10000);

    // Wait until the mirror is in sync with the observed hash map.
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

    // A change to the observed hash map must propagate through the mirror to the subscription.
    obs.insert(1000, "1000".to_string());
    assert_eq!(sub2.recv().await.unwrap(), Some(HashMapEvent::Set(1000, "1000".to_string())));

    obs.remove(&0);
    assert_eq!(sub2.recv().await.unwrap(), Some(HashMapEvent::Remove(0)));

    // When the original observed hash map ends, the subscription to the mirror must end as well.
    obs.done();
    assert_eq!(sub2.recv().await.unwrap(), Some(HashMapEvent::Done));
    assert!(sub2.is_done());
    assert_eq!(sub2.recv().await.unwrap(), None);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn mirrored_subscribe_incremental() {
    let mut obs: ObservableHashMap<_, _, remoc::codec::Default> = ObservableHashMap::new();
    for i in 0..100 {
        obs.insert(i, format!("{i}"));
    }

    let sub = obs.subscribe(1024);
    let mut mirror = sub.mirror(10000);

    // Wait until the mirror is in sync with the observed hash map.
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
    let mut hm2 = HashMap::new();
    loop {
        match sub2.recv().await.unwrap() {
            Some(HashMapEvent::Set(k, v)) => {
                hm2.insert(k, v);
            }
            Some(HashMapEvent::InitialComplete) => break,
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(hm2, *obs);
    assert!(sub2.is_complete());
    assert!(!sub2.is_done());

    // A change to the observed hash map must propagate through the mirror to the subscription.
    obs.insert(1000, "1000".to_string());
    assert_eq!(sub2.recv().await.unwrap(), Some(HashMapEvent::Set(1000, "1000".to_string())));

    obs.remove(&0);
    assert_eq!(sub2.recv().await.unwrap(), Some(HashMapEvent::Remove(0)));

    // When the original observed hash map ends, the subscription to the mirror must end as well.
    obs.done();
    assert_eq!(sub2.recv().await.unwrap(), Some(HashMapEvent::Done));
    assert!(sub2.is_done());
    assert_eq!(sub2.recv().await.unwrap(), None);
}
