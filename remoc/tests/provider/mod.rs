//! Type-erased ownership of remote object providers.

use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::Provider;

/// Time a provider is given to become done before the test fails.
const LIMIT: Duration = Duration::from_secs(2);

/// Providers of every kind can be held in one collection and awaited through it.
///
/// This exercises the dispatch of [`Provider::done`] over all its variants.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn done_completes_for_every_provider_kind() {
    crate::init();

    // The name of the kind is kept so that a failure identifies the variant.
    let mut providers: Vec<(&str, Provider)> = Vec::new();

    #[cfg(feature = "robj")]
    {
        use remoc::robj::{handle::Handle, lazy::Lazy, lazy_blob::LazyBlob};

        let (object, provider): (Handle<String>, _) = Handle::provided("value".to_string());
        providers.push(("handle", provider.into()));
        drop(object);

        let (object, provider): (Lazy<String>, _) = Lazy::provided("value".to_string());
        providers.push(("lazy", provider.into()));
        drop(object);

        let (object, provider): (LazyBlob, _) = LazyBlob::provided(bytes::Bytes::from_static(b"value"));
        providers.push(("lazy blob", provider.into()));
        drop(object);
    }

    #[cfg(feature = "rfn")]
    {
        use remoc::rfn::{RFn, RFnMut, RFnOnce};

        let (object, provider): (RFn<(u32,), u32>, _) = RFn::provided_1(|arg: u32| async move { arg });
        providers.push(("rfn", provider.into()));
        drop(object);

        let (object, provider): (RFnMut<(u32,), u32>, _) = RFnMut::provided_1(|arg: u32| async move { arg });
        providers.push(("rfn mut", provider.into()));
        drop(object);

        let (object, provider): (RFnOnce<(u32,), u32>, _) = RFnOnce::provided_1(|arg: u32| async move { arg });
        providers.push(("rfn once", provider.into()));
        drop(object);
    }

    assert!(!providers.is_empty(), "no provider kind was enabled");

    for (kind, provider) in providers.iter_mut() {
        timeout(LIMIT, provider.done())
            .await
            .unwrap_or_else(|_| panic!("the {kind} provider did not become done after its object was dropped"));
    }
}

/// Sends a provided handle to a remote endpoint and back, either keeping or
/// dropping its provider while the remote endpoint holds it.
///
/// Returns what the returned handle yields on this endpoint.
#[cfg(feature = "robj")]
async fn round_trip(keep: bool) -> Result<String, remoc::robj::handle::HandleError> {
    use remoc::robj::handle::Handle;

    let ((mut a_tx, mut a_rx), (mut b_tx, mut b_rx)) = crate::loop_channel::<Handle<String>>().await;

    let (handle, provider): (Handle<String>, _) = Handle::provided("value".to_string());
    let provider = Provider::from(provider);

    // Serializing the handle registers its value for the remote endpoint.
    a_tx.send(handle).await.unwrap();
    let remote = b_rx.recv().await.unwrap().unwrap();

    if keep {
        provider.keep();
    } else {
        drop(provider);
    }

    b_tx.send(remote).await.unwrap();
    let returned = a_rx.recv().await.unwrap().unwrap();
    returned.as_ref().await.map(|value| (*value).clone())
}

/// A kept provider keeps serving its value while a remote endpoint holds a handle.
#[cfg(feature = "robj")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn keep_leaves_the_value_available() {
    crate::init();

    let value = timeout(LIMIT, round_trip(true)).await.expect("the round trip blocked");
    assert_eq!(value.expect("a kept value became unavailable"), "value");
}

/// Dropping the provider stops the value from being served.
#[cfg(feature = "robj")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn dropping_the_provider_withdraws_the_value() {
    crate::init();

    let value = timeout(LIMIT, round_trip(false)).await.expect("the round trip blocked");
    assert!(value.is_err(), "a value stayed available after its provider was dropped");
}
