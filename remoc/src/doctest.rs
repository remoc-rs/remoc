use futures::Future;

#[cfg(feature = "rch")]
pub async fn loop_channel<T1, T2>() -> (
    (crate::rch::base::Sender<T1>, crate::rch::base::Receiver<T2>),
    (crate::rch::base::Sender<T2>, crate::rch::base::Receiver<T1>),
)
where
    T1: crate::RemoteSend,
    T2: crate::RemoteSend,
{
    use futures::StreamExt;

    let (transport_a_tx, transport_b_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(0);
    let (transport_b_tx, transport_a_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(0);

    let transport_a_rx = transport_a_rx.map(Ok::<_, std::io::Error>);
    let transport_b_rx = transport_b_rx.map(Ok::<_, std::io::Error>);

    let a = async move {
        let (conn, tx, rx) =
            crate::Connect::framed(Default::default(), transport_a_tx, transport_a_rx).await.unwrap();
        wokio::spawn(conn);
        (tx, rx)
    };

    let b = async move {
        let (conn, tx, rx) =
            crate::Connect::framed(Default::default(), transport_b_tx, transport_b_rx).await.unwrap();
        wokio::spawn(conn);
        (tx, rx)
    };

    tokio::join!(a, b)
}

#[cfg(feature = "rch")]
pub async fn client_server<T, ClientFut, ServerFut>(
    client: impl FnOnce(crate::rch::base::Sender<T>) -> ClientFut,
    server: impl FnOnce(crate::rch::base::Receiver<T>) -> ServerFut,
) where
    T: crate::RemoteSend,
    ClientFut: Future<Output = ()> + Send + 'static,
    ServerFut: Future<Output = ()> + Send + 'static,
{
    let ((a_tx, _a_rx), (_b_tx, b_rx)) = loop_channel::<_, ()>().await;
    tokio::join!(client(a_tx), server(b_rx));
}

#[cfg(feature = "rch")]
pub async fn client_server_bidir<T1, T2, ClientFut, ServerFut>(
    client: impl FnOnce(crate::rch::base::Sender<T1>, crate::rch::base::Receiver<T2>) -> ClientFut,
    server: impl FnOnce(crate::rch::base::Sender<T2>, crate::rch::base::Receiver<T1>) -> ServerFut,
) where
    T1: crate::RemoteSend,
    T2: crate::RemoteSend,
    ClientFut: Future<Output = ()> + Send + 'static,
    ServerFut: Future<Output = ()> + Send + 'static,
{
    let ((a_tx, a_rx), (b_tx, b_rx)) = loop_channel().await;
    tokio::join!(client(a_tx, a_rx), server(b_tx, b_rx));
}
