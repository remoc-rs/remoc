#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::rfn::{CallError, RFnMut};

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<RFnMut<_, _>>().await;

    let mut counter = 0;
    let rfn = RFnMut::new_2(move |arg1: i16, arg2: i16| {
        counter += arg1 + arg2;
        async move { Ok::<_, CallError>(counter) }
    });

    println!("Sending remote function");
    a_tx.send(rfn).await.unwrap();
    println!("Receiving remote function");
    let mut rfn = b_rx.recv().await.unwrap().unwrap();

    println!("calling function");
    let result = rfn.call(12, 13).await.unwrap();
    println!("rfn(12, 13) = {result}");
    assert_eq!(result, 25);

    println!("calling function");
    let result = rfn.call(33, 0).await.unwrap();
    println!("rfn(33, 0) = {result}");
    assert_eq!(result, 58);
}
