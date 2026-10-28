//! Establishing connections over the provided transports.

mod capabilities;
mod skip_fields;

use futures::{StreamExt, future::try_join};
use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use remoc::{Cfg, Connect, ConnectError};

/// A value large enough to be transferred in multiple chunks.
fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// A connection carries values in both directions and ends cleanly when the
/// remote endpoint goes away.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn framed_transport_round_trip() {
    crate::init();
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let a = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), a_tx, a_rx);
    let b = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), b_tx, b_rx);
    let ((a_conn, mut a_tx, mut a_rx), (b_conn, mut b_tx, mut b_rx)) = try_join(a, b).await.unwrap();

    wokio::spawn(async move {
        let _ = a_conn.await;
    });
    wokio::spawn(async move {
        let _ = b_conn.await;
    });

    let sent = payload(100_000);
    a_tx.send(sent.clone()).await.unwrap();
    assert_eq!(b_rx.recv().await.unwrap().unwrap(), sent, "value was altered in transfer");

    b_tx.send(sent.clone()).await.unwrap();
    assert_eq!(a_rx.recv().await.unwrap().unwrap(), sent, "value was altered in transfer");

    println!("Dropping one endpoint");
    drop(b_tx);
    drop(b_rx);
    assert!(a_rx.recv().await.unwrap().is_none(), "closure of the remote endpoint was not reported");
}

/// Endpoints agree on a common configuration, even when their configurations differ
/// substantially.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn differing_configurations_are_negotiated() {
    crate::init();
    crate::loop_transport!(0, a_tx, a_rx, b_tx, b_rx);

    let a_cfg = Cfg {
        chunk_size: 4,
        port_receive_buffer: 4,
        max_data_size: 1_000_000,
        shared_send_queue: 1,
        ..Default::default()
    };
    let b_cfg = Cfg {
        chunk_size: 60_000,
        port_receive_buffer: 1_000_000,
        max_data_size: 1_000_000,
        shared_send_queue: 16,
        ..Default::default()
    };

    let a = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(a_cfg, a_tx, a_rx);
    let b = Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(b_cfg, b_tx, b_rx);
    let ((a_conn, mut a_tx, mut a_rx), (b_conn, mut b_tx, mut b_rx)) = try_join(a, b).await.unwrap();

    wokio::spawn(async move {
        let _ = a_conn.await;
    });
    wokio::spawn(async move {
        let _ = b_conn.await;
    });

    // The receiving side determines the chunk size, thus both directions exercise
    // a different one.
    let sent = payload(200_000);
    a_tx.send(sent.clone()).await.unwrap();
    assert_eq!(b_rx.recv().await.unwrap().unwrap(), sent, "value was altered from small to large chunks");

    b_tx.send(sent.clone()).await.unwrap();
    assert_eq!(a_rx.recv().await.unwrap().unwrap(), sent, "value was altered from large to small chunks");
}

/// A loopback connection delivers what is sent over it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn loopback_delivers() {
    crate::init();
    let (conn, mut tx, mut rx) =
        Connect::loopback::<Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default()).await;
    wokio::spawn(async move {
        let _ = conn.await;
    });

    let sent = payload(10_000);
    tx.send(sent.clone()).await.unwrap();
    assert_eq!(rx.recv().await.unwrap().unwrap(), sent, "value was altered in loopback");
}

/// Connecting to a transport that does not speak the protocol fails instead of
/// waiting forever.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn garbage_transport_is_rejected() {
    crate::init();
    let (sink, _sink_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(16);
    let stream = futures::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"this is not a remoc connection")),
        Ok(bytes::Bytes::from_static(b"and neither is this")),
    ]);

    let res = timeout(
        Duration::from_secs(10),
        Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), sink, stream),
    )
    .await;

    match res {
        Ok(Err(ConnectError::ChMux(err))) => println!("Connecting failed with: {err}"),
        Ok(Err(err)) => println!("Connecting failed with: {err}"),
        Ok(Ok(_)) => panic!("connecting to a garbage transport succeeded"),
        Err(_) => panic!("connecting to a garbage transport did not return"),
    }
}

/// Connecting to a transport that never answers does not wait forever.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn silent_transport_times_out() {
    crate::init();
    let (sink, _sink_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(16);
    let (_stream_tx, stream_rx) = futures::channel::mpsc::channel::<bytes::Bytes>(16);
    let stream = stream_rx.map(Ok::<_, std::io::Error>);

    let cfg = Cfg { connection_timeout: Some(Duration::from_millis(500)), ..Default::default() };
    match timeout(
        Duration::from_secs(10),
        Connect::framed::<_, _, _, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(cfg, sink, stream),
    )
    .await
    {
        Ok(Err(err)) => println!("Connecting failed with: {err}"),
        Ok(Ok(_)) => panic!("connecting to a silent transport succeeded"),
        Err(_) => panic!("connecting to a silent transport did not honor the connection timeout"),
    }
}

/// Reader delivering only a few bytes per read, to exercise partial reads.
#[cfg(not(target_family = "wasm"))]
struct TrickleRead<R> {
    inner: R,
    max: usize,
}

#[cfg(not(target_family = "wasm"))]
impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for TrickleRead<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let max = self.max;
        let mut part = buf.take(max);
        std::task::ready!(std::pin::Pin::new(&mut self.inner).poll_read(cx, &mut part))?;
        let n = part.filled().len();
        // SAFETY: the inner reader has initialized the bytes it filled.
        unsafe { buf.assume_init(n) };
        buf.advance(n);
        std::task::Poll::Ready(Ok(()))
    }
}

/// Writer accepting only a few bytes per write, to exercise partial writes.
#[cfg(not(target_family = "wasm"))]
struct Trickle<W> {
    inner: W,
    max: usize,
    vectored: bool,
}

#[cfg(not(target_family = "wasm"))]
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Trickle<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let len = buf.len().min(self.max);
        std::pin::Pin::new(&mut self.inner).poll_write(cx, &buf[..len])
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if !self.vectored {
            let buf = bufs.iter().find(|buf| !buf.is_empty()).map_or(&[][..], |buf| &**buf);
            return self.poll_write(cx, buf);
        }

        let mut data = Vec::new();
        for buf in bufs {
            let n = buf.len().min(self.max - data.len());
            data.extend_from_slice(&buf[..n]);
        }
        std::pin::Pin::new(&mut self.inner).poll_write(cx, &data)
    }

    fn is_write_vectored(&self) -> bool {
        self.vectored
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Values of all sizes arrive intact over an IO transport whose writer accepts
/// only a few bytes at a time, with and without vectored writes, and whose
/// reader delivers whole buffers or only a few bytes at a time.
#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn io_transport_partial_writes() {
    crate::init();

    for (vectored, read_max) in [(false, usize::MAX), (true, usize::MAX), (false, 5), (true, 5)] {
        println!("Vectored writes: {vectored}, maximum read: {read_max}");
        let (a, b) = tokio::io::duplex(65_536);
        let (a_rd, a_wr) = tokio::io::split(a);
        let (b_rd, b_wr) = tokio::io::split(b);
        let a_rd = TrickleRead { inner: a_rd, max: read_max };
        let b_rd = TrickleRead { inner: b_rd, max: read_max };
        let a_wr = Trickle { inner: a_wr, max: 7, vectored };
        let b_wr = Trickle { inner: b_wr, max: 7, vectored };

        let a = Connect::io::<_, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), a_rd, a_wr);
        let b = Connect::io::<_, _, Vec<u8>, Vec<u8>, remoc::codec::Default>(Cfg::default(), b_rd, b_wr);
        let ((a_conn, mut a_tx, mut a_rx), (b_conn, mut b_tx, mut b_rx)) = try_join(a, b).await.unwrap();

        wokio::spawn(async move {
            let _ = a_conn.await;
        });
        wokio::spawn(async move {
            let _ = b_conn.await;
        });

        for size in [0, 1, 100, 4_095, 4_096, 4_097, 40_000, 300_000] {
            let sent = payload(size);
            a_tx.send(sent.clone()).await.unwrap();
            assert_eq!(b_rx.recv().await.unwrap().unwrap(), sent, "value of {size} bytes was altered");

            b_tx.send(sent.clone()).await.unwrap();
            assert_eq!(a_rx.recv().await.unwrap().unwrap(), sent, "value of {size} bytes was altered");
        }
    }
}
