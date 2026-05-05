//! Associated type referenced via the fully-qualified `<Self as Trait>::Item` form.

#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

#[remoc::rtc::remote]
pub trait Qualified {
    type Item: remoc::RemoteSend + Clone;

    // Argument and return type both use the fully-qualified projection.
    async fn echo(
        &self, value: <Self as Qualified>::Item,
    ) -> Result<<Self as Qualified>::Item, remoc::rtc::CallError>;
}

pub struct QualifiedObj;

impl Qualified for QualifiedObj {
    type Item = u64;

    async fn echo(&self, value: u64) -> Result<u64, remoc::rtc::CallError> {
        Ok(value)
    }
}

#[cfg_attr(not(feature = "js"), tokio::test)]
#[cfg_attr(feature = "js", wasm_bindgen_test)]
async fn simple() {
    use remoc::rtc::ServerRef;

    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<QualifiedClient<u64>>().await;

    let obj = QualifiedObj;
    let (server, client) = QualifiedServerRef::new(&obj, 1);

    a_tx.send(client).await.unwrap();

    let client_task = async move {
        let client = b_rx.recv().await.unwrap().unwrap();
        assert_eq!(client.echo(123).await.unwrap(), 123);
    };

    let ((), res) = tokio::join!(client_task, server.serve());
    res.unwrap();
}
