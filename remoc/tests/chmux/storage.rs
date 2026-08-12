use futures::{future::try_join, stream::StreamExt};
use std::time::Duration;

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_transport;
use remoc::{chmux, exec};

#[derive(Clone, Debug, PartialEq)]
struct Version(u32);

#[derive(Clone, Debug, PartialEq)]
struct Name(String);

fn cfg() -> chmux::Cfg {
    chmux::Cfg { connection_timeout: Some(Duration::from_secs(1)), ..Default::default() }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn value_storage() {
    crate::init();

    println!("Connecting...");
    loop_transport!(0, a_tx, a_rx, b_tx, b_rx);
    let ((a_mux, a_client, _a_server), (b_mux, _b_client, mut b_server)) =
        try_join(chmux::ChMux::new(cfg(), a_tx, a_rx), chmux::ChMux::new(cfg(), b_tx, b_rx)).await.unwrap();
    exec::spawn(async move { a_mux.run().await.unwrap() });
    exec::spawn(async move { b_mux.run().await.unwrap() });

    let connect = a_client.connect(a_client.connect_req().unwrap());
    let accept = async { b_server.accept().await.unwrap().unwrap() };
    let (connected, (_b_tx, _b_rx)) = futures::future::join(connect, accept).await;
    let (tx, _rx) = connected.unwrap();
    let storage = tx.storage();

    println!("Storing values");
    assert_eq!(storage.get::<Version>(), None);
    assert_eq!(storage.insert(Version(1)), None);
    assert_eq!(storage.insert(Version(2)), Some(Version(1)));
    assert_eq!(storage.get::<Version>(), Some(Version(2)));

    println!("Values of different types do not conflict");
    assert_eq!(storage.insert(Name("remoc".to_string())), None);
    assert_eq!(storage.get::<Version>(), Some(Version(2)));
    assert_eq!(storage.get::<Name>(), Some(Name("remoc".to_string())));

    println!("Accessing value by reference");
    assert_eq!(storage.with::<Name, _>(|name| name.0.len()), Some(5));
    assert_eq!(storage.with::<u64, _>(|value| *value), None);

    println!("Clones share the storage");
    let other = storage.clone();
    other.insert(Version(3));
    assert_eq!(storage.get::<Version>(), Some(Version(3)));

    println!("Removing values");
    assert_eq!(storage.remove::<Version>(), Some(Version(3)));
    assert_eq!(storage.get::<Version>(), None);
    assert_eq!(storage.remove::<Version>(), None);
    assert_eq!(storage.get::<Name>(), Some(Name("remoc".to_string())));
}

/// Value written into the storage before serialization takes place.
#[derive(Clone, Debug, PartialEq)]
struct Marker(u32);

/// Reads the marker value from the storage during serialization and deserialization.
#[derive(Debug, PartialEq)]
struct Probe {
    seen_by_sender: Option<u32>,
    seen_by_receiver: Option<u32>,
}

impl serde::Serialize for Probe {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let seen = remoc::rch::base::with_storage(|storage| storage.get::<Marker>()).flatten();
        serde::Serialize::serialize(&(seen.map(|m| m.0), self.seen_by_receiver), serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Probe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (seen_by_sender, _): (Option<u32>, Option<u32>) = serde::Deserialize::deserialize(deserializer)?;
        let seen_by_receiver = remoc::rch::base::with_storage(|storage| storage.get::<Marker>()).flatten();
        Ok(Self { seen_by_sender, seen_by_receiver: seen_by_receiver.map(|m| m.0) })
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn value_storage_during_serialization() {
    crate::init();

    let ((mut a_tx, _), (_, mut b_rx)) = crate::loop_channel::<Probe>().await;

    println!("Storing marker values before sending");
    a_tx.storage().insert(Marker(11));
    b_rx.storage().insert(Marker(22));

    a_tx.send(Probe { seen_by_sender: None, seen_by_receiver: None }).await.unwrap();
    let probe = b_rx.recv().await.unwrap().unwrap();
    println!("received: {probe:?}");

    assert_eq!(probe.seen_by_sender, Some(11), "serializer must see the sender-side storage");
    assert_eq!(probe.seen_by_receiver, Some(22), "deserializer must see the receiver-side storage");
}
