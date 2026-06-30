#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::{
    codec,
    robj::handle::{Handle, HandleError},
};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, mut a_rx), (mut b_tx, mut b_rx)) = loop_channel::<Handle<String>>().await;

    let value = "test string".to_string();

    let local_handle = Handle::new(value.clone());
    println!("Created handle: {:?}", local_handle);

    let _other_handle: Handle<_, codec::Default> = Handle::new(123);

    println!("Sending handle to remote");
    a_tx.send(local_handle).await.unwrap();
    println!("Receiving handle");
    let remote_handle = b_rx.recv().await.unwrap().unwrap();
    println!("{:?}", remote_handle);

    match remote_handle.as_ref().await {
        Ok(_) => panic!("remote deref succeeded"),
        Err(HandleError::Unknown) => (),
        Err(err) => panic!("wrong remote deref error: {err}"),
    }

    println!("Sending handle back");
    b_tx.send(remote_handle).await.unwrap();
    println!("Receiving handle");
    let local_handle = a_rx.recv().await.unwrap().unwrap();
    println!("{:?}", local_handle);

    println!("Changing handle type");
    let other_type_handle = local_handle.cast::<u32>();
    println!("{:?}", other_type_handle);

    match other_type_handle.as_ref().await {
        Ok(_) => panic!("mismatched type deref succeeded"),
        Err(HandleError::MismatchedType(t)) => println!("wrong type: {t}"),
        Err(err) => panic!("wrong mismatched type deref error: {err}"),
    }

    println!("Changing handle type back to original");
    let mut local_handle = other_type_handle.cast::<String>();

    assert_eq!(*local_handle.as_ref().await.unwrap(), value);
    assert_eq!(*local_handle.as_mut().await.unwrap(), value);

    println!("handle value ref: {}", *local_handle.as_ref().await.unwrap());
    println!("handle value mut: {}", *local_handle.as_mut().await.unwrap());
    println!("handle value: {}", local_handle.into_inner().await.unwrap());
}
