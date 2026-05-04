use rand::{Rng, RngExt};

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::robj::lazy_blob::LazyBlob;

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LazyBlob>().await;

    let mut rng = rand::rng();
    let size = rng.random_range(10_000_000..15_000_000);
    let mut data = vec![0; size];
    rng.fill_bytes(&mut data);

    println!("Creating lazy blob of size {} bytes", data.len());
    let lazy: LazyBlob = LazyBlob::new(data.clone().into());

    println!("Sending lazy blob");
    a_tx.send(lazy).await.unwrap();
    println!("Receiving lazy blob");
    let lazy = b_rx.recv().await.unwrap().unwrap();

    println!("Length is {} bytes", lazy.len().unwrap());
    assert_eq!(lazy.len().unwrap(), size);

    println!("Fetching reference");
    let fetched = lazy.get().await.unwrap();
    assert_eq!(Vec::from(fetched), data);

    println!("Fetching value");
    let fetched = lazy.into_inner().await.unwrap();
    assert_eq!(Vec::from(fetched), data);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn remote_forwarded() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LazyBlob>().await;
    let ((mut b_tx, _), (_, mut c_rx)) = loop_channel::<LazyBlob>().await;

    let mut rng = rand::rng();
    let size = rng.random_range(1_000_000..2_000_000);
    let mut data = vec![0; size];
    rng.fill_bytes(&mut data);

    println!("Creating lazy blob of size {} bytes", data.len());
    let lazy: LazyBlob = LazyBlob::new(data.clone().into());

    println!("A -> B: sending lazy blob");
    a_tx.send(lazy).await.unwrap();
    println!("A -> B: receiving lazy blob");
    let lazy = b_rx.recv().await.unwrap().unwrap();

    println!("Length at B is {} bytes", lazy.len().unwrap());
    assert_eq!(lazy.len().unwrap(), size);

    println!("B -> C: forwarding lazy blob");
    b_tx.send(lazy).await.unwrap();
    println!("B -> C: receiving lazy blob");
    let lazy = c_rx.recv().await.unwrap().unwrap();

    println!("Length at C is {} bytes", lazy.len().unwrap());
    assert_eq!(lazy.len().unwrap(), size);

    println!("Fetching reference");
    let fetched = lazy.get().await.unwrap();
    assert_eq!(Vec::from(fetched), data);

    println!("Fetching value");
    let fetched = lazy.into_inner().await.unwrap();
    assert_eq!(Vec::from(fetched), data);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn remote_forwarded_2hops() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<LazyBlob>().await;
    let ((mut b_tx, _), (_, mut c_rx)) = loop_channel::<LazyBlob>().await;
    let ((mut c_tx, _), (_, mut d_rx)) = loop_channel::<LazyBlob>().await;

    let mut rng = rand::rng();
    let size = rng.random_range(1_000_000..2_000_000);
    let mut data = vec![0; size];
    rng.fill_bytes(&mut data);

    println!("Creating lazy blob of size {} bytes", data.len());
    let lazy: LazyBlob = LazyBlob::new(data.clone().into());

    println!("A -> B");
    a_tx.send(lazy).await.unwrap();
    let lazy = b_rx.recv().await.unwrap().unwrap();
    assert_eq!(lazy.len().unwrap(), size);

    println!("B -> C");
    b_tx.send(lazy).await.unwrap();
    let lazy = c_rx.recv().await.unwrap().unwrap();
    assert_eq!(lazy.len().unwrap(), size);

    println!("C -> D");
    c_tx.send(lazy).await.unwrap();
    let lazy = d_rx.recv().await.unwrap().unwrap();
    assert_eq!(lazy.len().unwrap(), size);

    println!("Fetching reference at D");
    let fetched = lazy.get().await.unwrap();
    assert_eq!(Vec::from(fetched), data);

    println!("Fetching value at D");
    let fetched = lazy.into_inner().await.unwrap();
    assert_eq!(Vec::from(fetched), data);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn local() {
    crate::init();

    let mut rng = rand::rng();
    let size = rng.random_range(10_000..15_000);
    let mut data = vec![0; size];
    rng.fill_bytes(&mut data);

    println!("Creating lazy blob of size {} bytes", data.len());
    let lazy: LazyBlob = LazyBlob::new(data.clone().into());

    println!("Length is {} bytes", lazy.len().unwrap());
    assert_eq!(lazy.len().unwrap(), size);

    println!("Fetching reference");
    let fetched = lazy.get().await.unwrap();
    assert_eq!(Vec::from(fetched), data);

    println!("Fetching value");
    let fetched = lazy.into_inner().await.unwrap();
    assert_eq!(Vec::from(fetched), data);
}
