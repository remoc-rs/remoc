//! Tests for the io channel.
//!
//! Like the bin channel, at least one end of the io channel must be remote.
//! These tests send the receiver to remote and keep the sender local.

use rand::{Rng, RngExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;
use remoc::rch::io;

// ============================================================================
// Basic functionality tests
// ============================================================================

/// Test sized channel with simple send/receive.
/// Pattern: Send receiver to remote, keep sender local.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(11);

    // Verify size is known upfront
    assert_eq!(rx.size(), Some(11));
    assert_eq!(tx.expected_size(), Some(11));

    // Send receiver to remote
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Size should still be known after transport
    assert_eq!(rx.size(), Some(11));

    // Run write and read concurrently to avoid flow control blocking
    let write_task = wokio::spawn(async move {
        tx.write_all(b"hello world").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"hello world");
    assert_eq!(rx.bytes_received(), 11);
}

/// Test unsized channel with simple send/receive.
/// Pattern: Send receiver to remote, keep sender local.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unsized_simple() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::channel();

    // Size is unknown initially
    assert_eq!(rx.size(), None);
    assert_eq!(tx.expected_size(), None);

    // Send receiver to remote
    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Size still unknown
    assert_eq!(rx.size(), None);

    let write_task = wokio::spawn(async move {
        tx.write_all(b"hello world").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"hello world");
    assert_eq!(rx.bytes_received(), 11);
    // Size should now be known after EOF
    assert_eq!(rx.size(), Some(11));
}

/// Test sized channel with sender sent to remote.
/// Pattern: Send sender to remote, keep receiver local.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_send_sender() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Sender>().await;

    let (tx, mut rx) = io::sized(11);

    // Send sender to remote
    a_tx.send(tx).await.unwrap();
    let mut tx = b_rx.recv().await.unwrap().unwrap();

    // Run write and read concurrently
    let read_task = wokio::spawn(async move {
        let mut buf = Vec::new();
        rx.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"hello world");
        assert_eq!(rx.bytes_received(), 11);
    });

    tx.write_all(b"hello world").await.unwrap();
    tx.shutdown().await.unwrap();

    read_task.await.unwrap();
}

// ============================================================================
// Edge case: Size 0
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_zero() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(0);
    assert_eq!(rx.size(), Some(0));

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        // Should not be able to write anything, just shutdown
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert!(buf.is_empty());
    assert_eq!(rx.bytes_received(), 0);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unsized_zero() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::channel();

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        // Write nothing, just shutdown
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert!(buf.is_empty());
    assert_eq!(rx.bytes_received(), 0);
    assert_eq!(rx.size(), Some(0));
}

// ============================================================================
// Failure cases: Size mismatch
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_send_more_than_announced() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(5);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Start reader to consume data so writer can proceed until it fails
    let read_task = wokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = rx.read_to_end(&mut buf).await;
    });

    // Try to write more than announced - should fail
    let result = tx.write_all(b"hello world").await;
    assert!(result.is_err(), "writing more than announced should fail");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);

    read_task.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_send_less_than_announced_sender_shutdown() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(100);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Start reader
    let read_task = wokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = rx.read_to_end(&mut buf).await;
    });

    // Write less than announced
    tx.write_all(b"short").await.unwrap();
    // Shutdown should fail because we didn't write enough
    let result = tx.shutdown().await;
    assert!(result.is_err(), "shutdown with insufficient data should fail");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

    read_task.await.unwrap();
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_send_less_than_announced_receiver_eof() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(100);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Write less than announced
    tx.write_all(b"short").await.unwrap();
    // Drop sender without shutdown to simulate abnormal close
    drop(tx);

    // Receiver should fail with size mismatch
    let mut buf = Vec::new();
    let result = rx.read_to_end(&mut buf).await;
    assert!(result.is_err(), "receiver should fail on size mismatch");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

// ============================================================================
// Failure cases: Unsized without shutdown
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unsized_no_shutdown() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::channel();

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Write some data but don't shutdown
    tx.write_all(b"hello").await.unwrap();
    // Drop sender without shutdown
    drop(tx);

    // Receiver should fail because size was never sent
    let mut buf = Vec::new();
    let result = rx.read_to_end(&mut buf).await;
    assert!(result.is_err(), "receiver should fail without proper shutdown");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
}

// ============================================================================
// Large data transfer
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_large_data() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let mut rng = rand::rng();
    let size: usize = rng.random_range(100_000..500_000);
    let mut data = vec![0u8; size];
    rng.fill_bytes(&mut data);

    let (mut tx, rx) = io::sized(size as u64);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let data_clone = data.clone();
    let write_task = wokio::spawn(async move {
        tx.write_all(&data_clone).await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf.len(), size);
    assert_eq!(buf, data);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unsized_large_data() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let mut rng = rand::rng();
    let size: usize = rng.random_range(100_000..500_000);
    let mut data = vec![0u8; size];
    rng.fill_bytes(&mut data);

    let (mut tx, rx) = io::channel();

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let data_clone = data.clone();
    let write_task = wokio::spawn(async move {
        tx.write_all(&data_clone).await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf.len(), size);
    assert_eq!(buf, data);
    assert_eq!(rx.size(), Some(size as u64));
}

// ============================================================================
// Multiple writes
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_multiple_writes() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(15);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        tx.write_all(b"hello").await.unwrap();
        tx.write_all(b" ").await.unwrap();
        tx.write_all(b"world").await.unwrap();
        tx.write_all(b"!!!!").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"hello world!!!!");
    assert_eq!(rx.bytes_received(), 15);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unsized_multiple_writes() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::channel();

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        tx.write_all(b"one ").await.unwrap();
        tx.write_all(b"two ").await.unwrap();
        tx.write_all(b"three").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"one two three");
}

// ============================================================================
// Bytes tracking
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn bytes_tracking() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::channel();

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    assert_eq!(tx.bytes_written(), 0);
    assert_eq!(rx.bytes_received(), 0);

    // Writer in background
    let write_task = wokio::spawn(async move {
        tx.write_all(b"12345").await.unwrap();
        assert_eq!(tx.bytes_written(), 5);
        tx.write_all(b"67890").await.unwrap();
        assert_eq!(tx.bytes_written(), 10);
        tx.shutdown().await.unwrap();
    });

    let mut buf = [0u8; 5];
    rx.read_exact(&mut buf).await.unwrap();
    assert_eq!(rx.bytes_received(), 5);

    rx.read_exact(&mut buf).await.unwrap();
    assert_eq!(rx.bytes_received(), 10);

    write_task.await.unwrap();
}

// ============================================================================
// Forwarding
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_forward() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;
    let ((mut c_tx, _), (_, mut d_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(11);

    // Send to first remote
    a_tx.send(rx).await.unwrap();
    let rx = b_rx.recv().await.unwrap().unwrap();

    // Forward to second remote
    c_tx.send(rx).await.unwrap();
    let mut rx = d_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        tx.write_all(b"hello world").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"hello world");
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn unsized_forward() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;
    let ((mut c_tx, _), (_, mut d_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::channel();

    // Size unknown initially
    assert_eq!(rx.size(), None);

    // Send to first remote
    a_tx.send(rx).await.unwrap();
    let rx = b_rx.recv().await.unwrap().unwrap();

    // Forward to second remote
    c_tx.send(rx).await.unwrap();
    let mut rx = d_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        tx.write_all(b"hello world").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"hello world");
    // Size should now be known after EOF
    assert_eq!(rx.size(), Some(11));
}

// ============================================================================
// Empty write
// ============================================================================

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn empty_write() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(5);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        // Empty write should be a no-op
        tx.write_all(b"").await.unwrap();
        tx.write_all(b"hello").await.unwrap();
        tx.write_all(b"").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let mut buf = Vec::new();
    rx.read_to_end(&mut buf).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(buf, b"hello");
}

// ============================================================================
// Partial read
// ============================================================================

/// Test partial reads with sized channel using read_exact.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn sized_partial_reads() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let (mut tx, rx) = io::sized(10);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Check remaining before any reads
    assert_eq!(rx.remaining(), Some(10));

    let write_task = wokio::spawn(async move {
        tx.write_all(b"0123456789").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    // Read in small chunks
    let mut buf = [0u8; 3];
    rx.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"012");
    assert_eq!(rx.bytes_received(), 3);
    assert_eq!(rx.remaining(), Some(7));

    let mut buf = [0u8; 4];
    rx.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"3456");
    assert_eq!(rx.bytes_received(), 7);
    assert_eq!(rx.remaining(), Some(3));

    let mut buf = [0u8; 3];
    rx.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"789");
    assert_eq!(rx.bytes_received(), 10);
    assert_eq!(rx.remaining(), Some(0));

    // Next read should return EOF (0 bytes)
    let mut buf = [0u8; 1];
    let n = rx.read(&mut buf).await.unwrap();
    assert_eq!(n, 0);

    write_task.await.unwrap();
}

// ============================================================================
// File transfer simulation
// ============================================================================

/// Simulates a file transfer use case where metadata and io::Receiver are sent together.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn file_transfer_simulation() {
    use serde::{Deserialize, Serialize};

    crate::init();

    /// File transfer request - demonstrates embedding io::Receiver in a struct.
    #[derive(Debug, Serialize, Deserialize)]
    struct FileTransfer {
        filename: String,
        size: u64,
        data: io::Receiver,
    }

    let ((mut client_tx, _), (_, mut server_rx)) = loop_channel::<FileTransfer>().await;

    // Simulate file content
    let file_content = b"This is the content of the file being transferred.\n".repeat(100);
    let file_size = file_content.len() as u64;

    // Create io channel with known size
    let (mut io_tx, io_rx) = io::sized(file_size);

    // Client sends transfer request with embedded receiver
    client_tx
        .send(FileTransfer { filename: "example.txt".to_string(), size: file_size, data: io_rx })
        .await
        .unwrap();

    // Client writes file content (in background to avoid deadlock)
    let file_content_clone = file_content.clone();
    let write_task = wokio::spawn(async move {
        // Simulate chunked file reading
        for chunk in file_content_clone.chunks(1024) {
            io_tx.write_all(chunk).await.unwrap();
        }
        io_tx.shutdown().await.unwrap();
    });

    // Server receives transfer request
    let transfer = server_rx.recv().await.unwrap().unwrap();
    assert_eq!(transfer.filename, "example.txt");
    assert_eq!(transfer.size, file_size);

    // Server can check size from receiver too
    let mut data_rx = transfer.data;
    assert_eq!(data_rx.size(), Some(file_size));
    assert_eq!(data_rx.remaining(), Some(file_size));

    // Server reads file content
    let mut received = Vec::new();
    data_rx.read_to_end(&mut received).await.unwrap();

    // Verify
    assert_eq!(received.len() as u64, file_size);
    assert_eq!(received, file_content);
    assert_eq!(data_rx.bytes_received(), file_size);
    assert_eq!(data_rx.remaining(), Some(0));

    write_task.await.unwrap();
}

/// Test with unknown size - useful for streaming content of unknown length.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn streaming_transfer_unknown_size() {
    use serde::{Deserialize, Serialize};

    crate::init();

    /// Streaming data request - size unknown upfront.
    #[derive(Debug, Serialize, Deserialize)]
    struct StreamTransfer {
        name: String,
        data: io::Receiver,
    }

    let ((mut client_tx, _), (_, mut server_rx)) = loop_channel::<StreamTransfer>().await;

    // Create io channel with unknown size
    let (mut io_tx, io_rx) = io::channel();

    // Client sends transfer request
    client_tx.send(StreamTransfer { name: "live_stream".to_string(), data: io_rx }).await.unwrap();

    // Client writes data in chunks (simulating live data)
    let write_task = wokio::spawn(async move {
        for i in 0..10 {
            let chunk = format!("Chunk {}\n", i);
            io_tx.write_all(chunk.as_bytes()).await.unwrap();
        }
        io_tx.shutdown().await.unwrap();
    });

    // Server receives
    let transfer = server_rx.recv().await.unwrap().unwrap();
    assert_eq!(transfer.name, "live_stream");

    let mut data_rx = transfer.data;
    // Size unknown initially
    assert_eq!(data_rx.size(), None);
    assert_eq!(data_rx.remaining(), None);

    // Read all data
    let mut received = Vec::new();
    data_rx.read_to_end(&mut received).await.unwrap();

    // After EOF, size becomes known
    assert!(data_rx.size().is_some());
    assert_eq!(data_rx.remaining(), Some(0));

    // Verify content
    let expected: String = (0..10).map(|i| format!("Chunk {}\n", i)).collect();
    assert_eq!(String::from_utf8(received).unwrap(), expected);

    write_task.await.unwrap();
}

// ============================================================================
// tokio::io::copy integration tests
// ============================================================================

/// Test that tokio::io::copy works with io channel.
/// Important: tokio::io::copy does NOT call shutdown - caller must do it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tokio_copy_integration() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let data = b"Hello from tokio::io::copy!".repeat(100);
    let size = data.len() as u64;

    let (mut tx, rx) = io::sized(size);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let data_clone = data.clone();
    let write_task = wokio::spawn(async move {
        let mut reader = std::io::Cursor::new(data_clone);

        // tokio::io::copy just copies bytes, does NOT call shutdown
        let copied = tokio::io::copy(&mut reader, &mut tx).await.unwrap();
        assert_eq!(copied, size);

        // We MUST call shutdown ourselves
        tx.shutdown().await.unwrap();
    });

    // Read using tokio::io::copy to a Vec
    let mut output = Vec::new();
    let received = tokio::io::copy(&mut rx, &mut output).await.unwrap();

    write_task.await.unwrap();

    assert_eq!(received, size);
    assert_eq!(output, data);
}

/// Test sized channel behavior: receiver uses size as contract, not EOF.
/// With sized(N), once N bytes are received, transfer is complete.
/// Shutdown is still recommended for cleanup but receiver doesn't require it.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tokio_copy_sized_no_shutdown_succeeds() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let data = b"data without explicit shutdown";
    let size = data.len() as u64;

    let (mut tx, rx) = io::sized(size);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        let mut reader = std::io::Cursor::new(data);
        tokio::io::copy(&mut reader, &mut tx).await.unwrap();
        // NOT calling shutdown - for sized channels this still works
        // because size IS the contract
        drop(tx);
    });

    // For sized channels, receiver succeeds once it reads the expected bytes
    let mut output = Vec::new();
    let result = tokio::io::copy(&mut rx, &mut output).await;
    assert!(result.is_ok());
    assert_eq!(output, data);

    write_task.await.unwrap();
}

/// Test unsized channel: NOT calling shutdown causes receiver error.
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tokio_copy_unsized_no_shutdown_fails() {
    crate::init();
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let data = b"data without proper shutdown";

    let (mut tx, rx) = io::channel(); // unsized!

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    let write_task = wokio::spawn(async move {
        let mut reader = std::io::Cursor::new(data);
        tokio::io::copy(&mut reader, &mut tx).await.unwrap();
        // NOT calling shutdown - for unsized, this causes receiver error
        drop(tx);
    });

    // Receiver should fail because size was never sent
    let mut output = Vec::new();
    let result = tokio::io::copy(&mut rx, &mut output).await;
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);

    write_task.await.unwrap();
}

/// Test behavior when read error occurs during copy.
/// Important: if read fails, shutdown must NOT be called!
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn tokio_copy_read_error_no_shutdown() {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::AsyncRead;

    crate::init();

    /// A reader that fails after reading some bytes.
    struct FailingReader {
        data: Vec<u8>,
        pos: usize,
        fail_at: usize,
    }

    impl AsyncRead for FailingReader {
        fn poll_read(
            mut self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.pos >= self.fail_at {
                return Poll::Ready(Err(std::io::Error::other("simulated read error")));
            }
            let remaining = self.data.len() - self.pos;
            let to_read = remaining.min(buf.remaining()).min(self.fail_at - self.pos);
            if to_read > 0 {
                buf.put_slice(&self.data[self.pos..self.pos + to_read]);
                self.pos += to_read;
            }
            Poll::Ready(Ok(()))
        }
    }

    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<io::Receiver>().await;

    let full_data = b"0123456789".repeat(100); // 1000 bytes
    let size = full_data.len() as u64;

    let (mut tx, rx) = io::sized(size);

    a_tx.send(rx).await.unwrap();
    let mut rx = b_rx.recv().await.unwrap().unwrap();

    // Writer: read from failing source, DO NOT shutdown on error
    let write_task = wokio::spawn(async move {
        let mut reader = FailingReader {
            data: full_data.clone(),
            pos: 0,
            fail_at: 500, // Fail after 500 bytes
        };

        let result = tokio::io::copy(&mut reader, &mut tx).await;

        // Read failed!
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Other);

        // IMPORTANT: We do NOT call shutdown because read failed.
        // Just drop the sender - this signals error to receiver.
        drop(tx);
    });

    // Receiver: should fail because not enough data was sent
    let mut output = Vec::new();
    let result = tokio::io::copy(&mut rx, &mut output).await;

    // Receiver gets error (channel closed with size mismatch)
    assert!(result.is_err());

    write_task.await.unwrap();
}
