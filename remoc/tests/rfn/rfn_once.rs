#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::rfn::{CallError, RFnOnce};

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RFnOnce<_, Result<String, CallError>>>().await;

    let reply_value = "reply".to_string();
    let fn_value = reply_value.clone();
    let rfn = RFnOnce::new_1(|arg: i16| async move {
        assert_eq!(arg, 123);
        Ok(fn_value)
    });

    println!("Sending remote function");
    a_tx.send(rfn).await.unwrap();
    println!("Receiving remote function");
    let rfn = b_rx.recv().await.unwrap().unwrap();

    println!("calling function");
    let result = rfn.call(123).await.unwrap();
    println!("result: {result}");
    assert_eq!(result, reply_value);
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn not_called() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RFnOnce<_, Result<String, CallError>>>().await;

    let rfn = RFnOnce::new_1(|_arg: i16| async move { Ok("reply".to_string()) });

    println!("Sending remote function");
    a_tx.send(rfn).await.unwrap();
    println!("Receiving remote function");
    let rfn = b_rx.recv().await.unwrap().unwrap();

    println!("Dropping remote function without calling it");
    drop(rfn);
}
