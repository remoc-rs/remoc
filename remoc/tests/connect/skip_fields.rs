//! Transported types that leave struct fields holding their default value out of the
//! serialized data.
//!
//! Whether a field may be left out depends on the codec in use and on what the remote
//! endpoint announced it can decode, so this exercises the combinations end-to-end:
//! a codec that can omit fields ([`Postbag`]), one that cannot ([`PostbagSlim`]) and a
//! remote endpoint announcing it is unable to decode omitted fields.
//!
//! Every message carries both a field holding its default value, which is left out, and
//! one holding a custom value, which is not. A field that is wrongly left out is thus
//! caught by a wrong value, and a message the remote endpoint cannot decode by a failing
//! transfer.

use futures::StreamExt;
use std::time::Duration;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{
    Cfg, Connect,
    chmux::Capabilities,
    codec::{Postbag, PostbagSlim},
    rch::{DEFAULT_BUFFER, DEFAULT_MAX_ITEM_SIZE, mpsc, watch},
};

/// A rate limit that is not the default and thus serialized.
const RATE_LIMIT: Duration = Duration::from_millis(250);

/// A maximum item size that is not the default and thus serialized.
const MAX_ITEM_SIZE: usize = 4096;

/// Channels carrying settings that hold their default value, which is left out of the
/// serialized data, next to ones holding a custom value, which is not.
type Payload<C> = (
    watch::Receiver<u32, C>,
    watch::Receiver<u32, C>,
    mpsc::Receiver<u32, C>,
    mpsc::Receiver<u32, C, DEFAULT_BUFFER, MAX_ITEM_SIZE>,
);

/// Transports the channels to an endpoint announcing the specified capabilities and
/// verifies that their settings arrive unchanged.
async fn check<C>(remote: Capabilities)
where
    C: remoc::codec::Codec,
{
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let a_cfg = Cfg::default();
    let b_cfg = Cfg { capabilities: remote, ..Default::default() };

    let a = Connect::framed::<_, _, _, _, Payload<C>, Payload<C>, C>(a_cfg, a_tx, a_rx);
    let b = Connect::framed::<_, _, _, _, Payload<C>, Payload<C>, C>(b_cfg, b_tx, b_rx);
    let ((a_conn, mut a_tx, _a_rx), (b_conn, _b_tx, mut b_rx)) = futures::future::try_join(a, b).await.unwrap();
    wokio::spawn(async move {
        let _ = a_conn.await;
    });
    wokio::spawn(async move {
        let _ = b_conn.await;
    });

    // The rate limit is left out when it holds the default value of its type.
    let (default_rate_tx, default_rate_rx) = watch::channel::<u32, C>(0);
    let (custom_rate_tx, mut custom_rate_rx) = watch::channel::<u32, C>(0);
    custom_rate_rx.set_rate_limit(RATE_LIMIT);

    // The maximum item size is left out when it holds the default of the *field*, which
    // is not the default value of its type.
    let (default_size_tx, default_size_rx) = mpsc::channel::<u32, C>();
    let (custom_size_tx, custom_size_rx) = mpsc::channel::<u32, C>();
    let custom_size_rx = custom_size_rx.set_max_item_size::<MAX_ITEM_SIZE>();

    a_tx.send((default_rate_rx, custom_rate_rx, default_size_rx, custom_size_rx)).await.unwrap();
    let (default_rate_rx, custom_rate_rx, default_size_rx, custom_size_rx) = b_rx.recv().await.unwrap().unwrap();

    assert_eq!(default_rate_rx.rate_limit(), Duration::ZERO, "left out rate limit");
    assert_eq!(custom_rate_rx.rate_limit(), RATE_LIMIT, "serialized rate limit");
    assert_eq!(default_size_rx.remote_max_item_size(), Some(DEFAULT_MAX_ITEM_SIZE), "left out maximum item size");
    assert_eq!(custom_size_rx.remote_max_item_size(), Some(MAX_ITEM_SIZE), "serialized maximum item size");

    // The channels must still work, not just carry their settings.
    default_rate_tx.send(7).unwrap();
    custom_rate_tx.send(7).unwrap();
    default_size_tx.send(42).await.unwrap();
    custom_size_tx.send(42).await.unwrap();
}

/// A codec that can leave out fields holding their default value.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn postbag() {
    crate::init();
    check::<Postbag>(Capabilities::default()).await;
}

/// A codec that encodes a struct as a plain sequence of its fields and thus cannot leave
/// any of them out.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn postbag_slim() {
    crate::init();
    check::<PostbagSlim>(Capabilities::default()).await;
}

/// An endpoint announcing that it cannot decode data leaving out struct fields, which
/// must stop the other endpoint from leaving any out.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn remote_endpoint_without_skipping() {
    crate::init();
    check::<Postbag>(Capabilities { postbag_allow_skip: false, ..Default::default() }).await;
}
