#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::robj::rw_lock::{Owner, RwLock};

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RwLock<String>>().await;

    let value = "test string".to_string();
    let new_value = "new value".to_string();

    println!("Creating owner");
    let owner = Owner::new(value.clone());

    println!("Sending RwLocks");
    a_tx.send(owner.rw_lock()).await.unwrap();
    a_tx.send(owner.rw_lock()).await.unwrap();

    println!("Receiving RwLocks");
    let rw_lock1 = b_rx.recv().await.unwrap().unwrap();
    let rw_lock2 = b_rx.recv().await.unwrap().unwrap();

    {
        let read1 = rw_lock1.read().await.unwrap();
        let read2 = rw_lock2.read().await.unwrap();
        println!("Read value 1: {}", *read1);
        println!("Read value 2: {}", *read1);
        assert_eq!(*read1, value);
        assert_eq!(*read2, value);

        assert!(!read1.is_invalidated());
        assert!(!read2.is_invalidated());

        let rw_lock3 = rw_lock1.clone();
        let read3 = rw_lock3.read().await.unwrap();
        println!("Read value 3: {}", *read3);
        assert_eq!(*read3, value);
        assert!(!read3.is_invalidated());
        drop(read3);

        println!("Making write request");
        let write_req = wokio::spawn(async move { rw_lock3.write().await.unwrap() });

        println!("Waiting for invalidation");
        read1.invalidated().await;
        assert!(read1.is_invalidated());
        println!("read1 invalidated");
        read2.invalidated().await;
        assert!(read2.is_invalidated());
        println!("read2 invalidated");

        drop(read1);
        drop(read2);

        write_req.await.unwrap();
    }

    println!("Making write request");
    let mut write1 = rw_lock1.write().await.unwrap();
    *write1 = new_value.clone();
    println!("Committing");
    write1.commit().await.unwrap();

    let read1 = rw_lock1.read().await.unwrap();
    let read2 = rw_lock2.read().await.unwrap();
    println!("Read value: {}", *read1);
    assert_eq!(*read1, new_value);
    assert_eq!(*read2, new_value);

    assert!(!read1.is_invalidated());
    assert!(!read2.is_invalidated());
}
