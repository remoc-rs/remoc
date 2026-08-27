//! Cancellation of chunked messages and its effect on flow control.

use std::time::Duration;
use wokio::time::timeout;

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use super::{cfg, connected_with_cfg, spawn_forwarder};
use remoc::chmux::{self, Received, RecvChunkError};

/// Bytes of a cancelled message per round.
const CHUNK: usize = 16;
const CHUNKS_PER_ROUND: usize = 2;

/// Rounds of cancelling, so many that a channel leaking flow control credits for
/// cancelled data comes to a halt.
const ROUNDS: usize = 50;

/// A receive buffer small enough that leaking the credits of a few cancelled
/// messages exhausts it.
fn tight_cfg() -> chmux::Cfg {
    chmux::Cfg {
        chunk_size: CHUNK as u32,
        port_receive_buffer: (CHUNK * 4) as u32,
        // Messages larger than this are delivered chunk by chunk, so that abandoning
        // one is observable rather than being reassembled and discarded silently.
        max_data_size: 8,
        ..cfg()
    }
}

/// Starts a chunked message, sends chunks and abandons it without finishing.
///
/// The message is cancelled once the next message begins.
async fn cancel_message(tx: &mut chmux::Sender) {
    let mut chunks = tx.send_chunks();
    for _ in 0..CHUNKS_PER_ROUND {
        chunks = chunks.send(vec![1u8; CHUNK].into()).await.unwrap();
    }
    drop(chunks);

    tx.send("next".into()).await.unwrap();
}

/// Receives an abandoned message and the message that follows it.
async fn receive_cancelled(rx: &mut chmux::Receiver) {
    match rx.recv_any().await.unwrap() {
        Some(Received::Chunks) => (),
        other => panic!("unexpected receive result: {other:?}"),
    }

    loop {
        match rx.recv_chunk().await {
            Ok(Some(_)) => (),
            Ok(None) => panic!("an abandoned message was reported as complete"),
            Err(RecvChunkError::Cancelled) => break,
            Err(err) => panic!("receiving chunks failed: {err}"),
        }
    }

    // A cancellation is also reported when no message follows, thus the receiver of a
    // cancelled message cannot know whether one does and receives the next message.
    assert_eq!(recv_message(rx).await, b"next");
}

/// Receives a complete message, however it is delivered.
async fn recv_message(rx: &mut chmux::Receiver) -> Vec<u8> {
    match rx.recv_any().await.unwrap() {
        Some(Received::Data(data)) => Vec::from(data),
        Some(Received::Chunks) => {
            let mut received = Vec::new();
            loop {
                match rx.recv_chunk().await {
                    Ok(Some(chunk)) => received.extend_from_slice(&chunk),
                    Ok(None) => break received,
                    Err(err) => panic!("receiving chunks failed: {err}"),
                }
            }
        }
        other => panic!("unexpected receive result: {other:?}"),
    }
}

/// Repeatedly cancelling chunked messages must not consume the flow control credits
/// of the channel, which would make it stall.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn cancelling_messages_does_not_exhaust_credits() {
    crate::init();
    let (client, mut listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut a_tx, _a_rx) = client.connect_port().await.unwrap();
    let (_b_tx, mut b_rx) = listener.accept().await.unwrap().unwrap();

    for round in 0..ROUNDS {
        let res = timeout(Duration::from_secs(10), async {
            cancel_message(&mut a_tx).await;
            receive_cancelled(&mut b_rx).await;
        })
        .await;
        assert!(res.is_ok(), "the channel stalled after {round} cancelled messages");
    }

    println!("Verifying that the channel still transfers a message larger than its buffer");
    let sent = vec![7u8; CHUNK * 16];
    let transfer = timeout(Duration::from_secs(10), async {
        a_tx.send(sent.clone().into()).await.unwrap();
        recv_message(&mut b_rx).await
    })
    .await
    .expect("the channel stalled after cancelling messages");
    assert_eq!(transfer, sent);
}

/// The same, but through a forwarder, which cancels the onward message when the
/// message it is forwarding is cancelled.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn cancelling_forwarded_messages_does_not_exhaust_credits() {
    crate::init();
    let (a_client, mut b_a_listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (b_c_client, mut c_listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;

    let (mut a_tx, _a_rx) = a_client.connect_port().await.unwrap();
    let b_a = b_a_listener.accept().await.unwrap().unwrap();
    let b_c = b_c_client.connect_port().await.unwrap();
    spawn_forwarder(b_a, b_c);
    let (_c_tx, mut c_rx) = c_listener.accept().await.unwrap().unwrap();

    for round in 0..ROUNDS {
        let res = timeout(Duration::from_secs(10), async {
            cancel_message(&mut a_tx).await;
            receive_cancelled(&mut c_rx).await;
        })
        .await;
        assert!(res.is_ok(), "forwarding stalled after {round} cancelled messages");
    }

    println!("Verifying that forwarding still transfers a message larger than the buffer");
    let sent = vec![7u8; CHUNK * 16];
    let transfer = timeout(Duration::from_secs(10), async {
        a_tx.send(sent.clone().into()).await.unwrap();
        recv_message(&mut c_rx).await
    })
    .await
    .expect("forwarding stalled after cancelling messages");
    assert_eq!(transfer, sent);
}

/// Receiving a message that is still incomplete must report it as chunked again,
/// instead of losing the chunks that follow.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn receiving_resumes_an_incomplete_message() {
    crate::init();
    let (client, mut listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut a_tx, _a_rx) = client.connect_port().await.unwrap();
    let (_b_tx, mut b_rx) = listener.accept().await.unwrap().unwrap();

    const CHUNKS: usize = 3;
    let sender = async {
        let mut chunks = a_tx.send_chunks();
        for i in 0..CHUNKS - 1 {
            chunks = chunks.send(vec![i as u8; CHUNK].into()).await.unwrap();
        }
        chunks.send_final(vec![(CHUNKS - 1) as u8; CHUNK].into()).await.unwrap();
    };

    let receiver = async {
        match b_rx.recv_any().await.unwrap() {
            Some(Received::Chunks) => (),
            other => panic!("unexpected receive result: {other:?}"),
        }

        // Receive only the first chunk, so that the message is under way but nothing
        // of it is buffered any more.
        let first = b_rx.recv_chunk().await.unwrap().expect("no chunk was received");
        assert_eq!(first.len(), CHUNK);

        // Asking for a message again must report the one that is still incomplete.
        match b_rx.recv_any().await.unwrap() {
            Some(Received::Chunks) => (),
            other => panic!("an incomplete message was not reported as chunked: {other:?}"),
        }

        let mut rest = 1;
        loop {
            match b_rx.recv_chunk().await {
                Ok(Some(chunk)) => {
                    assert_eq!(chunk.len(), CHUNK);
                    rest += 1;
                }
                Ok(None) => break,
                Err(err) => panic!("receiving chunks failed: {err}"),
            }
        }
        assert_eq!(rest, CHUNKS, "chunks of the incomplete message were lost");
    };

    timeout(Duration::from_secs(10), futures::future::join(sender, receiver))
        .await
        .expect("receiving an incomplete message stalled");
}

/// A message exceeding the maximum data size must be dropped by `recv`, so that the
/// message following it can be received.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn recv_drops_an_oversized_message() {
    crate::init();
    let (client, mut listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut a_tx, _a_rx) = client.connect_port().await.unwrap();
    let (_b_tx, mut b_rx) = listener.accept().await.unwrap().unwrap();

    let sender = async {
        a_tx.send(vec![1u8; CHUNK * 4].into()).await.unwrap();
        a_tx.send("next".into()).await.unwrap();
    };

    let receiver = async {
        match b_rx.recv().await {
            Err(chmux::RecvError::ExceedsMaxDataSize(_)) => (),
            other => panic!("oversized message was not rejected: {other:?}"),
        }

        let next = b_rx.recv().await.unwrap().expect("no message followed the oversized one");
        assert_eq!(Vec::from(next), b"next");
    };

    timeout(Duration::from_secs(10), futures::future::join(sender, receiver))
        .await
        .expect("receiving after an oversized message stalled");
}

/// Discarding the chunks of a message that was cancelled by the sender must not
/// swallow the message that follows it, since that one has already begun.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn discarding_chunks_after_cancellation_keeps_next_message() {
    crate::init();
    let (client, mut listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut a_tx, _a_rx) = client.connect_port().await.unwrap();
    let (_b_tx, mut b_rx) = listener.accept().await.unwrap().unwrap();

    let sender = async { cancel_message(&mut a_tx).await };

    let receiver = async {
        match b_rx.recv_any().await.unwrap() {
            Some(Received::Chunks) => (),
            other => panic!("unexpected receive result: {other:?}"),
        }

        loop {
            match b_rx.recv_chunk().await {
                Ok(Some(_)) => (),
                Ok(None) => panic!("an abandoned message was reported as complete"),
                Err(RecvChunkError::Cancelled) => break,
                Err(err) => panic!("receiving chunks failed: {err}"),
            }
        }

        // The chunked reception has already ended, so this must not discard the
        // message that the sender started when it cancelled.
        b_rx.discard_chunks();

        assert_eq!(recv_message(&mut b_rx).await, b"next");
    };

    timeout(Duration::from_secs(10), futures::future::join(sender, receiver))
        .await
        .expect("receiving after discarding chunks stalled");
}

/// A chunked reception that is abandoned by discarding its chunks must not have its
/// remainder reported as a new message.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn discarding_chunks_skips_the_rest_of_the_message() {
    crate::init();
    let (client, mut listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut a_tx, _a_rx) = client.connect_port().await.unwrap();
    let (_b_tx, mut b_rx) = listener.accept().await.unwrap().unwrap();

    const CHUNKS: usize = 4;
    let sender = async {
        let mut chunks = a_tx.send_chunks();
        for i in 0..CHUNKS - 1 {
            chunks = chunks.send(vec![i as u8; CHUNK].into()).await.unwrap();
        }
        chunks.send_final(vec![(CHUNKS - 1) as u8; CHUNK].into()).await.unwrap();

        a_tx.send("next".into()).await.unwrap();
    };

    let receiver = async {
        match b_rx.recv_any().await.unwrap() {
            Some(Received::Chunks) => (),
            other => panic!("unexpected receive result: {other:?}"),
        }

        // Receive one chunk and abandon the rest of the message.
        b_rx.recv_chunk().await.unwrap().expect("no chunk was received");
        b_rx.discard_chunks();

        assert_eq!(recv_message(&mut b_rx).await, b"next");
    };

    timeout(Duration::from_secs(10), futures::future::join(sender, receiver))
        .await
        .expect("receiving after discarding chunks stalled");
}

/// A message that is forwarded only partially, because forwarding it fails, must not
/// have its remainder reported as a new message by the receiver.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn failed_forwarding_discards_the_partial_message() {
    crate::init();
    let (client, mut listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut a_tx, _a_rx) = client.connect_port().await.unwrap();
    let (_b_tx, mut b_rx) = listener.accept().await.unwrap().unwrap();

    // A second connection whose receiving end is dropped, so that forwarding to it fails.
    let (fwd_client, mut fwd_listener) = connected_with_cfg(tight_cfg(), tight_cfg()).await;
    let (mut fwd_tx, _fwd_rx) = fwd_client.connect_port().await.unwrap();
    let (far_tx, far_rx) = fwd_listener.accept().await.unwrap().unwrap();
    drop(far_rx);
    drop(far_tx);
    fwd_tx.closed().await;

    const CHUNKS: usize = 4;
    let sender = async {
        let mut chunks = a_tx.send_chunks();
        for i in 0..CHUNKS - 1 {
            chunks = chunks.send(vec![i as u8; CHUNK].into()).await.unwrap();
        }
        chunks.send_final(vec![(CHUNKS - 1) as u8; CHUNK].into()).await.unwrap();
    };

    let receiver = async {
        b_rx.forward(&mut fwd_tx).await.expect_err("forwarding to a dropped receiver succeeded");

        // The remainder of the partially forwarded message must be discarded, so that
        // nothing more is received on this channel.
        match timeout(Duration::from_millis(500), b_rx.recv_any()).await {
            Err(_elapsed) => (),
            Ok(other) => panic!("remainder of the partially forwarded message was received: {other:?}"),
        }
    };

    timeout(Duration::from_secs(10), futures::future::join(sender, receiver)).await.expect("forwarding stalled");
}
