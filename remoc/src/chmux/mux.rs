use bytes::Bytes;
use futures::{
    Future, FutureExt, future, pin_mut,
    sink::{Sink, SinkExt},
    stream::{Stream, StreamExt},
};
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    error::Error,
    fmt,
    marker::PhantomData,
    mem::size_of,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::{mpsc, mpsc::Permit, oneshot},
    try_join,
};
use tokio_util::sync::ReusableBoxFuture;

use super::{
    AnyStorage, Cfg, ChMuxError, PROTOCOL_VERSION, PROTOCOL_VERSION_PORT_ID, PortReq,
    client::{Client, ConnectRequest, ConnectResponse},
    credit::{ChannelCreditMonitor, CreditProvider, credit_monitor_pair, credit_send_pair},
    listener::{Listener, RemoteConnectMsg, Request},
    msg::{ExchangedCfg, MultiplexMsg},
    port_allocator::{PortAllocator, PortNumber},
    receiver::{PortReceiveMsg, ReceivedData, ReceivedPortRequests, Receiver},
    sender::Sender,
};
use crate::exec::time::{sleep, timeout};

/// Multiplexer protocol error.
fn protocol_err<SinkError, StreamError>(msg: impl AsRef<str>) -> super::ChMuxError<SinkError, StreamError> {
    super::ChMuxError::Protocol(msg.as_ref().to_string())
}

/// Port state.
#[derive(Debug)]
enum PortState {
    /// OpenPort request has been initiated locally and is waiting for reply
    /// from remote endpoint.
    Connecting {
        /// Channel for providing the response to the local requester.
        response_tx: oneshot::Sender<ConnectResponse>,
    },
    /// Port is connected.
    Connected {
        /// Remote port.
        remote_port: u32,
        /// Credit provider for sending.
        /// Initially present, None when Hangup message has been received.
        sender_credit_provider: CreditProvider,
        /// Receive queue.
        receiver_tx_data: Option<mpsc::UnboundedSender<PortReceiveMsg>>,
        /// Channel-specific credit monitor for received data.
        receiver_credit_monitor: ChannelCreditMonitor,
        /// Local receiver has been closed and ReceiveClose message has been sent to remote endpoint.
        receiver_closed: bool,
        /// Local receiver has been dropped and ReceiveFinish message has been sent to remote endpoint,
        /// thus no more port credits can be returned.
        receiver_dropped: bool,
        /// Sender has been dropped and SendFinish message has been sent to remote endpoint,
        /// thus no more data can be send from this port to remote endpoint.
        sender_dropped: bool,
        /// Remote receiver has been closed, thus no more data should be sent to remote port.
        remote_receiver_closed: Arc<AtomicBool>,
        /// Notification senders for when remote receiver has been closed.
        remote_receiver_closed_notify: Arc<std::sync::Mutex<Option<Vec<oneshot::Sender<()>>>>>,
        /// Remote receiver has been dropped, thus no more sent data will be processed and
        /// no port credits will be returned.
        remote_receiver_dropped: bool,
    },
}

/// Event from a port to the multiplexer event loop.
#[derive(Debug)]
pub(crate) enum PortEvt {
    /// Connection has been accepted.
    Accepted {
        /// Allocated local port.
        local_port: PortNumber,
        /// Remote port.
        remote_port: u32,
        /// Reply with port sender and receiver.
        port_tx: oneshot::Sender<(Sender, Receiver)>,
    },
    /// Connection was rejected.
    Rejected {
        /// Remote port.
        remote_port: u32,
        /// True if rejection due to no ports available.
        no_ports: bool,
    },
    /// Send message with content.
    SendData {
        /// Remote port that will receive data.
        remote_port: u32,
        /// Data to send.
        data: Bytes,
        /// First chunk of data.
        first: bool,
        /// Last chunk of data.
        last: bool,
    },
    /// Send ports.
    SendPorts {
        /// Remote port that will receive ports.
        remote_port: u32,
        /// First chunk of ports.
        first: bool,
        /// Last chunk of ports.
        last: bool,
        /// Wait for remote port to become available.
        wait: bool,
        /// Ports to send, together with response channel.
        ports: Vec<(PortReq, oneshot::Sender<ConnectResponse>)>,
    },
    /// Return channel-specific flow control credits.
    ReturnCredits {
        /// Remote port that will receive credits.
        remote_port: u32,
        /// Number of credits in bytes.
        credits: u32,
    },
    /// Sender has been dropped.
    SenderDropped {
        /// Local port.
        local_port: u32,
    },
    /// Receiver has been closed, i.e. remote endpoint should stop sending messages on this channel.
    ReceiverClosed {
        /// Local port.
        local_port: u32,
    },
    /// Receiver has been dropped.
    ReceiverDropped {
        /// Local port.
        local_port: u32,
    },
}

// Global event.
#[derive(Debug)]
enum GlobalEvt {
    /// Connect request from local client.
    ConnectReq(ConnectRequest),
    /// All local clients have been dropped.
    AllClientsDropped,
    /// Local listener has been dropped.
    ListenerDropped,
    /// Event from an open port.
    Port(PortEvt),
    /// Send Goodbye message.
    SendGoodbye,
}

/// Message with optionally associated data.
#[derive(Debug)]
struct TransportMsg {
    /// Message.
    msg: MultiplexMsg,
    /// Data chunk, if message is data message.
    data: Option<Bytes>,
}

impl TransportMsg {
    /// Message only.
    fn new(msg: MultiplexMsg) -> Self {
        assert!(!matches!(&msg, &MultiplexMsg::Data { .. }), "MultiplexMsg::Data with missing data");
        Self { msg, data: None }
    }

    /// Message with data.
    fn with_data(msg: MultiplexMsg, data: Bytes) -> Self {
        assert!(matches!(&msg, &MultiplexMsg::Data { .. }), "MultiplexMsg with unexpected data");
        Self { msg, data: Some(data) }
    }
}

/// Channel multiplexer.
#[must_use = "You must call run() on the ChMux object for the connection to work."]
pub struct ChMux<TransportSink, TransportStream> {
    /// Our configuration.
    local_cfg: Cfg,
    /// Remote configuration.
    remote_cfg: ExchangedCfg,
    /// Remote protocol version.
    remote_protocol_version: u8,
    /// Channel for connection requests from local client.
    connect_rx: Option<mpsc::UnboundedReceiver<ConnectRequest>>,
    /// Channels for connection requests from remote endpoint with wait set and not set.
    listen_tx: Option<(mpsc::Sender<RemoteConnectMsg>, mpsc::Sender<RemoteConnectMsg>)>,
    /// Port allocator.
    port_allocator: PortAllocator,
    /// Open local ports.
    ports: HashMap<PortNumber, PortState>,
    /// Outstanding requests by the remote endpoint for connecting ports.
    outstanding_remote_port_requests: HashSet<u32>,
    /// Sender from channels to event loop.
    channel_tx: mpsc::Sender<PortEvt>,
    /// Channel receiver of event loop.
    channel_rx: Option<mpsc::Receiver<PortEvt>>,
    /// Force termination request.
    terminate_rx: Option<mpsc::UnboundedReceiver<()>>,
    /// All user clients have been dropped.
    all_clients_dropped: bool,
    /// Remote client has been dropped.
    remote_client_dropped: bool,
    /// Remote listener has been dropped.
    remote_listener_dropped: Arc<AtomicBool>,
    /// Goodbye message has been sent.
    goodbye_sent: bool,
    /// Goodbye message has been received.
    goodbye_received: bool,
    /// Transport sender.
    transport_sink: Option<TransportSink>,
    /// Transport receiver.
    transport_stream: Option<TransportStream>,
    /// Storage.
    storage: AnyStorage,
}

impl<TransportSink, TransportStream> fmt::Debug for ChMux<TransportSink, TransportStream> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ChMux")
            .field("local_cfg", &self.local_cfg)
            .field("remote_cfg", &self.remote_cfg)
            .field("local_protocol_version", &PROTOCOL_VERSION)
            .field("remote_protocol_version", &self.remote_protocol_version)
            .finish()
    }
}

impl<TransportSink, TransportSinkError, TransportStream, TransportStreamError>
    ChMux<TransportSink, TransportStream>
where
    TransportSink: Sink<Bytes, Error = TransportSinkError> + Send + Unpin,
    TransportSinkError: Error + Send + Sync + 'static,
    TransportStream: Stream<Item = Result<Bytes, TransportStreamError>> + Send + Unpin,
    TransportStreamError: Error + Send + Sync + 'static,
{
    /// Creates a new multiplexer.
    ///
    /// After creation use the `run` method of the multiplexer to launch the dispatch task.
    ///
    /// # Panics
    /// Panics if specified configuration does not obey limits documented in [Cfg].
    #[tracing::instrument(level = "trace", skip_all, fields(cfg))]
    pub async fn new(
        cfg: Cfg, mut transport_sink: TransportSink, mut transport_stream: TransportStream,
    ) -> Result<(Self, Client, Listener), ChMuxError<TransportSinkError, TransportStreamError>> {
        // Check configuration.
        cfg.check();

        // Say hello to remote endpoint and exchange configurations.
        let fut = Self::exchange_hello(&cfg, &mut transport_sink, &mut transport_stream);
        let (remote_protocol_version, remote_cfg) = match cfg.connection_timeout {
            Some(dur) => timeout(dur, fut).await.map_err(|_| ChMuxError::Timeout)??,
            None => fut.await?,
        };

        // Create channels.
        let (channel_tx, channel_rx) = mpsc::channel(cfg.shared_send_queue);
        let (listen_wait_tx, listen_wait_rx) = mpsc::channel(usize::from(cfg.connect_queue) + 1);
        let (listen_no_wait_tx, listen_no_wait_rx) = mpsc::channel(usize::from(cfg.connect_queue) + 1);
        let (connect_tx, connect_rx) = mpsc::unbounded_channel();
        let (terminate_tx, terminate_rx) = mpsc::unbounded_channel();

        // Create user objects.
        let port_allocator = PortAllocator::new(cfg.max_ports);
        let remote_listener_dropped = Arc::new(AtomicBool::new(false));
        let multiplexer = ChMux {
            remote_protocol_version,
            local_cfg: cfg,
            remote_cfg: remote_cfg.clone(),
            connect_rx: Some(connect_rx),
            listen_tx: Some((listen_wait_tx, listen_no_wait_tx)),
            port_allocator: port_allocator.clone(),
            ports: HashMap::new(),
            outstanding_remote_port_requests: HashSet::new(),
            channel_tx,
            channel_rx: Some(channel_rx),
            terminate_rx: Some(terminate_rx),
            remote_client_dropped: false,
            remote_listener_dropped: remote_listener_dropped.clone(),
            all_clients_dropped: false,
            goodbye_sent: false,
            goodbye_received: false,
            transport_sink: Some(transport_sink),
            transport_stream: Some(transport_stream),
            storage: AnyStorage::new(),
        };

        let client = Client::new(
            connect_tx,
            remote_cfg.connect_queue,
            port_allocator.clone(),
            remote_listener_dropped,
            terminate_tx.clone(),
        );
        let listener = Listener::new(listen_wait_rx, listen_no_wait_rx, port_allocator, terminate_tx);

        Ok((multiplexer, client, listener))
    }

    /// Feed transport message to sink and log it.
    #[tracing::instrument(level = "trace", skip_all, fields(msg=?msg.msg, data=?msg.data))]
    async fn feed_msg(
        msg: TransportMsg, sink: &mut TransportSink,
    ) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        sink.feed(msg.msg.to_vec().into()).await.map_err(ChMuxError::SinkError)?;

        if let Some(data) = msg.data {
            sink.feed(data).await.map_err(ChMuxError::SinkError)?;
        }

        Ok(())
    }

    /// Flush sink and log it.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn flush(sink: &mut TransportSink) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        sink.flush().await.map_err(ChMuxError::SinkError)
    }

    /// Receive message and log it.
    #[tracing::instrument(level = "trace", skip_all, fields(msg, data))]
    async fn recv_msg(
        stream: &mut TransportStream,
    ) -> Result<TransportMsg, ChMuxError<TransportSinkError, TransportStreamError>> {
        let msg_data = match stream.next().await {
            Some(Ok(msg_data)) => msg_data,
            Some(Err(err)) => return Err(ChMuxError::StreamError(err)),
            None => return Err(ChMuxError::StreamClosed),
        };

        let msg = MultiplexMsg::from_slice(&msg_data)?;

        let data = if let MultiplexMsg::Data { .. } = &msg {
            match stream.next().await {
                Some(Ok(data)) => Some(data),
                Some(Err(err)) => return Err(ChMuxError::StreamError(err)),
                None => return Err(ChMuxError::StreamClosed),
            }
        } else {
            None
        };

        tracing::Span::current().record("msg", tracing::field::debug(&msg));
        if let Some(data) = &data {
            tracing::Span::current().record("data", tracing::field::debug(&data));
        }

        Ok(TransportMsg { msg, data })
    }

    /// Exchange Hello message with remote endpoint.
    #[tracing::instrument(level = "trace", skip_all)]
    async fn exchange_hello(
        cfg: &Cfg, sink: &mut TransportSink, stream: &mut TransportStream,
    ) -> Result<(u8, ExchangedCfg), ChMuxError<TransportSinkError, TransportStreamError>> {
        // Say hello to remote endpoint and send our configuration.
        let send_task = async {
            Self::feed_msg(TransportMsg::new(MultiplexMsg::Reset), sink).await?;
            Self::flush(sink).await?;
            Self::feed_msg(
                TransportMsg::new(MultiplexMsg::Hello { version: PROTOCOL_VERSION, cfg: cfg.into() }),
                sink,
            )
            .await?;
            Self::flush(sink).await?;
            Ok(())
        };

        // Receive hello and configuration from remote endpoint.
        let recv_task = async {
            loop {
                match Self::recv_msg(stream).await {
                    Ok(TransportMsg { msg: MultiplexMsg::Hello { version, cfg }, .. }) => {
                        break Ok((version, cfg));
                    }
                    Ok(_) => (),
                    Err(ChMuxError::Protocol(_)) => (),
                    Err(err) => return Err(err),
                }
            }
        };

        Ok(try_join!(send_task, recv_task)?.1)
    }

    /// Returns true, when multiplexer task should terminate because no more
    /// requests are possible.
    fn should_terminate(&self) -> bool {
        let mut terminate = true;

        // Ensures that all ports are closed on both sides.
        terminate &= self.ports.is_empty();
        // Ensures that local clients are all dropped or remote listener is dropped.
        terminate &= self.all_clients_dropped || self.remote_listener_dropped.load(Ordering::Relaxed);
        // Ensures that local listener or all remote clients are dropped.
        terminate &= self.listen_tx.is_none() || self.remote_client_dropped;
        // No remote port requests are outstanding.
        terminate &= self.outstanding_remote_port_requests.is_empty();
        // If goodbye has been sent, we request connection termination,
        // possibly even with still connected ports.
        terminate |= self.goodbye_sent;
        // If goodbye has been received, remote endpoint requests connection termination,
        // possibly even with still connected ports.
        terminate |= self.goodbye_received;

        if terminate {
            tracing::trace!("should terminate");
        }

        terminate
    }

    /// Create port in port registry and return associated sender and receiver.
    #[tracing::instrument(level = "trace", skip(self))]
    fn create_port(&mut self, local_port: PortNumber, remote_port: u32) -> (Sender, Receiver) {
        let local_port_num = *local_port;

        let sender_tx = self.channel_tx.clone();
        let (sender_credit_provider, sender_credit_user) = credit_send_pair(self.remote_cfg.port_receive_buffer);

        let receiver_tx = self.channel_tx.clone();
        let (receiver_tx_data, receiver_rx_data) = mpsc::unbounded_channel();
        let (receiver_credit_monitor, receiver_credit_returner) =
            credit_monitor_pair(self.local_cfg.receive_buffer);

        let hangup_notify = Arc::new(std::sync::Mutex::new(Some(Vec::new())));
        let hangup_recved = Arc::new(AtomicBool::new(false));

        if let Some(PortState::Connected { remote_port, .. }) = self.ports.insert(
            local_port,
            PortState::Connected {
                remote_port,
                sender_credit_provider,
                receiver_tx_data: Some(receiver_tx_data),
                receiver_credit_monitor,
                remote_receiver_closed_notify: hangup_notify.clone(),
                remote_receiver_closed: hangup_recved.clone(),
                receiver_closed: false,
                receiver_dropped: false,
                sender_dropped: false,
                remote_receiver_dropped: false,
            },
        ) {
            panic!(
                "create_port called for local port {local_port_num} already connected to remote port {remote_port}"
            );
        }

        let sender = Sender::new(
            local_port_num,
            remote_port,
            self.remote_cfg.chunk_size as usize,
            self.local_cfg.max_data_size,
            sender_tx,
            sender_credit_user,
            Arc::downgrade(&hangup_recved),
            Arc::downgrade(&hangup_notify),
            self.port_allocator.clone(),
            self.storage.clone(),
        );

        let receiver = Receiver::new(
            local_port_num,
            remote_port,
            self.local_cfg.max_data_size,
            self.local_cfg.max_received_ports,
            receiver_tx,
            receiver_rx_data,
            receiver_credit_returner,
            self.port_allocator.clone(),
            self.storage.clone(),
        );

        (sender, receiver)
    }

    /// Releases a port if no more local requests to it are possible
    /// and no more messages from the remote endpoint can reference it.
    fn maybe_free_port(&mut self, local_port: u32) {
        let mut free = true;

        if let Some(PortState::Connected {
            receiver_tx_data,
            receiver_dropped,
            sender_dropped,
            remote_receiver_dropped,
            ..
        }) = self.ports.get(&local_port)
        {
            // Ensures that local sender is dropped and thus no more
            // Data and SendFinish messages can be sent for port.
            free &= *sender_dropped;
            // Ensures that local receiver is dropped and thus no more
            // PortCredits, ReceiveClose and ReceiveFinish messages can be sent for port.
            free &= *receiver_dropped;
            // Ensures that remote sender has been dropped and thus no more
            // Data and SendFinish messages can be received for port.
            free &= receiver_tx_data.is_none();
            // Ensures that remote receiver is dropped and thus no more
            // PortCredits, ReceiveClose and ReceiveFinish messages can be received for port.
            free &= *remote_receiver_dropped;
        } else {
            panic!("maybe_free_port called for port {} not in connected state.", &local_port);
        }

        if free {
            tracing::trace!(local_port, "freed port");
            self.ports.remove(&local_port);
        }
    }

    /// Sends data over the transport sink.
    ///
    /// Automatically sends pings if no data is to be transmitted.
    async fn send_task(
        mut sink: &mut TransportSink, ping_interval: Option<Duration>, mut rx: mpsc::Receiver<TransportMsg>,
    ) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        async fn get_next_ping(ping_interval: Option<Duration>) {
            match ping_interval {
                Some(interval) => sleep(interval).await,
                None => future::pending().await,
            }
        }

        let mut next_ping = ReusableBoxFuture::new(get_next_ping(ping_interval));
        let mut need_flush = false;

        loop {
            SinkReady::new(&mut sink).await.map_err(ChMuxError::SinkError)?;

            tokio::select! {
                biased;

                msg_opt = rx.recv() => {
                    match msg_opt {
                        Some(msg) => {
                            let is_goodbye = matches!(&msg, TransportMsg {msg: MultiplexMsg::Goodbye, ..});

                            Self::feed_msg(msg, sink).await?;

                            if is_goodbye {
                                break;
                            }

                            next_ping.set(get_next_ping(ping_interval));
                            need_flush = true;
                        }
                        None => break,
                    }
                }

                () = &mut next_ping => {
                    Self::feed_msg(TransportMsg::new(MultiplexMsg::Ping), sink).await?;
                    next_ping.set(get_next_ping(ping_interval));
                    need_flush = true;
                }

                () = future::ready(()), if need_flush => {
                    Self::flush(sink).await?;
                    need_flush = false;
                }
            }
        }

        // Flushing may fail after Goodbye message has been sent, because the remote
        // endpoint may immediately close the connection.
        let _ = Self::flush(sink).await;

        Ok(())
    }

    /// Receives data over the transport sink.
    ///
    /// Watches the connection timeout.
    async fn recv_task(
        stream: &mut TransportStream, connection_timeout: Option<Duration>, tx: mpsc::Sender<TransportMsg>,
    ) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        async fn get_connection_timeout(connection_timeout: Option<Duration>) {
            match connection_timeout {
                Some(timeout) => sleep(timeout).await,
                None => future::pending().await,
            }
        }

        let mut next_timeout = ReusableBoxFuture::new(get_connection_timeout(connection_timeout));

        while let Ok(tx_permit) = tx.reserve().await {
            tokio::select! {
                biased;

                msg = Self::recv_msg(stream) => {
                    let msg = msg?;
                    let is_goodbye = matches!(&msg, TransportMsg {msg: MultiplexMsg::Goodbye, ..});
                    tx_permit.send(msg);
                    if is_goodbye {
                        break;
                    }

                    next_timeout.set(get_connection_timeout(connection_timeout));
                },

                () = &mut next_timeout => return Err(ChMuxError::Timeout),
            }
        }

        Ok(())
    }

    /// Runs the multiplexer dispatcher.
    ///
    /// The dispatcher terminates when the client, server and all channels have been dropped or
    /// the transport is closed.
    #[tracing::instrument(name = "remoc::chmux", level = "debug", skip_all, ret)]
    pub async fn run(mut self) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        let mut transport_sink = self.transport_sink.take().unwrap();
        let mut transport_stream = self.transport_stream.take().unwrap();

        // Create send over transport task.
        let (send_tx, send_rx) = mpsc::channel(self.local_cfg.transport_send_queue);
        let send_task =
            Self::send_task(&mut transport_sink, self.remote_cfg.connection_timeout.map(|d| d / 2), send_rx)
                .fuse();
        pin_mut!(send_task);

        // Create receive over transport task.
        let (recv_tx, mut recv_rx) = mpsc::channel(self.local_cfg.transport_receive_queue);
        let recv_task = Self::recv_task(&mut transport_stream, self.local_cfg.connection_timeout, recv_tx).fuse();
        pin_mut!(recv_task);

        // Setup channels.
        let mut channel_rx = self.channel_rx.take().unwrap();
        let mut connect_rx = self.connect_rx.take().unwrap();
        let mut terminate_rx = self.terminate_rx.take().unwrap();
        let mut send_task_ended = false;

        while !(self.goodbye_sent && self.goodbye_received && send_task_ended) {
            let send_prep_task = async {
                // Obtain permit to ensure that space is available in transport send queue.
                let permit = match send_tx.reserve().await {
                    Ok(permit) => permit,
                    Err(_) => return None,
                };

                // Select local request for processing.
                let event = tokio::select! {
                    biased;

                    // Server dropped.
                    () = async { match &self.listen_tx {
                        Some((listen_wait_tx, _)) => listen_wait_tx.closed().await,
                        None => future::pending().await
                    }} => {
                        // listen_no_wait_tx is closed simultaneously.
                        GlobalEvt::ListenerDropped
                    },

                    // Connection request from client.
                    connect_req_opt = connect_rx.recv(), if !self.all_clients_dropped => {
                        match connect_req_opt {
                            Some(connect_req) => GlobalEvt::ConnectReq(connect_req),
                            None => GlobalEvt::AllClientsDropped,
                        }
                    },

                    // Request from port.
                    Some(msg) = channel_rx.recv() => {
                        GlobalEvt::Port(msg)
                    },

                    // Local request to terminate forcibly.
                    Some(()) = terminate_rx.recv(), if !self.goodbye_sent => {
                        GlobalEvt::SendGoodbye
                    }

                    // Send Goodbye message and terminate.
                    () = future::ready(()), if self.should_terminate() && !self.goodbye_sent => {
                        GlobalEvt::SendGoodbye
                    },
                };

                Some((permit, event))
            };

            tokio::select! {
                // Local send request.
                Some((permit, event)) = send_prep_task => self.handle_event(permit, event).await?,

                // Received message from remote endpoint.
                Some(msg) = recv_rx.recv() => self.handle_received_msg(msg).await?,

                // Send task ended.
                res = &mut send_task => {
                    match res {
                        Ok(()) => send_task_ended = true,
                        Err(err) => return Err(err),
                    }
                }

                // Receive task failed.
                Err(err) = &mut recv_task => return Err(err),
            }
        }

        Ok(())
    }

    /// Handle local event that results in sending a message to the remote endpoint.
    #[tracing::instrument(level = "trace", skip_all, fields(event=?event))]
    async fn handle_event(
        &mut self, permit: Permit<'_, TransportMsg>, event: GlobalEvt,
    ) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        let send_msg = |permit: Permit<'_, TransportMsg>, msg: MultiplexMsg| {
            tracing::trace!(op="send", msg=?msg);
            permit.send(TransportMsg::new(msg))
        };

        match event {
            // Process local connect request.
            GlobalEvt::ConnectReq(ConnectRequest { local_port, id, sent_tx: _sent_tx, response_tx, wait }) => {
                if !self.remote_listener_dropped.load(Ordering::Relaxed) {
                    let local_port_num = *local_port;
                    if self.ports.insert(local_port, PortState::Connecting { response_tx }).is_some() {
                        panic!("ConnectRequest for already used local port {local_port_num}");
                    }
                    let id = (self.remote_protocol_version >= PROTOCOL_VERSION_PORT_ID).then_some(id);
                    send_msg(permit, MultiplexMsg::OpenPort { client_port: local_port_num, wait, id });
                } else {
                    let _ = response_tx.send(ConnectResponse::Rejected { no_ports: false });
                }
            }

            // Remote connect request was accepted by local listener.
            GlobalEvt::Port(PortEvt::Accepted { local_port, remote_port, port_tx }) => {
                if !self.outstanding_remote_port_requests.remove(&remote_port) {
                    panic!("Accepted non-outstanding remote port {remote_port} request");
                }
                let local_port_num = *local_port;
                send_msg(
                    permit,
                    MultiplexMsg::PortOpened { client_port: remote_port, server_port: local_port_num },
                );
                let (sender, receiver) = self.create_port(local_port, remote_port);
                let _ = port_tx.send((sender, receiver));
            }

            // Remote connect request was rejected by local listener.
            GlobalEvt::Port(PortEvt::Rejected { remote_port, no_ports }) => {
                if !self.outstanding_remote_port_requests.remove(&remote_port) {
                    panic!("Rejected non-outstanding remote port {remote_port} request");
                }
                send_msg(permit, MultiplexMsg::Rejected { client_port: remote_port, no_ports });
            }

            // Send data from port.
            GlobalEvt::Port(PortEvt::SendData { remote_port, data, first, last }) => {
                let msg = MultiplexMsg::Data { port: remote_port, first, last };
                tracing::trace!(op="send", msg=?msg, data=?&data);
                permit.send(TransportMsg::with_data(msg, data));
            }

            // Send ports from port.
            GlobalEvt::Port(PortEvt::SendPorts { remote_port, ports, first, last, wait }) => {
                let mut port_nums = Vec::new();
                let mut ids = (self.remote_protocol_version >= PROTOCOL_VERSION_PORT_ID).then_some(Vec::new());
                for (PortReq { port, id }, response_tx) in ports {
                    let port_num = *port;
                    if self.ports.insert(port, PortState::Connecting { response_tx }).is_some() {
                        panic!("SendPorts with already used local port {port_num}");
                    }
                    port_nums.push(port_num);
                    if let Some(ids) = &mut ids {
                        ids.push(id);
                    }
                }
                send_msg(
                    permit,
                    MultiplexMsg::PortData { port: remote_port, first, last, wait, ports: port_nums, ids },
                );
            }

            // Return port credits to remote endpoint.
            GlobalEvt::Port(PortEvt::ReturnCredits { remote_port, credits }) => {
                send_msg(permit, MultiplexMsg::PortCredits { port: remote_port, credits });
            }

            // Local port sender has been dropped.
            GlobalEvt::Port(PortEvt::SenderDropped { local_port }) => {
                if let Some(PortState::Connected { remote_port, sender_dropped, .. }) =
                    self.ports.get_mut(&local_port)
                {
                    if *sender_dropped {
                        panic!("PortEvt SenderDropped more than once for port {}", &local_port);
                    }
                    *sender_dropped = true;
                    send_msg(permit, MultiplexMsg::SendFinish { port: *remote_port });
                    self.maybe_free_port(local_port);
                } else {
                    panic!("PortEvt SenderDropped for port {} in invalid state", &local_port);
                }
            }

            // Local port receiver has been closed.
            GlobalEvt::Port(PortEvt::ReceiverClosed { local_port }) => {
                if let Some(PortState::Connected { remote_port, receiver_closed, receiver_dropped, .. }) =
                    self.ports.get_mut(&local_port)
                {
                    if *receiver_closed || *receiver_dropped {
                        panic!(
                            "PortEvt ReceiverClosed or ReceiverDropped more than once for port {}",
                            &local_port
                        );
                    }
                    *receiver_closed = true;
                    send_msg(permit, MultiplexMsg::ReceiveClose { port: *remote_port });
                } else {
                    panic!("PortEvt ReceiverClosed for non-connected port {}", &local_port);
                }
            }

            // Local port receiver has been dropped.
            // No port credits can be returned afterwards.
            GlobalEvt::Port(PortEvt::ReceiverDropped { local_port }) => match self.ports.get_mut(&local_port) {
                Some(PortState::Connected { remote_port, receiver_dropped, .. }) => {
                    if *receiver_dropped {
                        panic!("PortEvt ReceiverDropped more than once for port {}.", &local_port);
                    }
                    *receiver_dropped = true;
                    send_msg(permit, MultiplexMsg::ReceiveFinish { port: *remote_port });
                    self.maybe_free_port(local_port);
                }
                _ => {
                    panic!("PortEvt ReceiverDropped for port {} in invalid state.", &local_port);
                }
            },

            // Process that all clients has been dropped.
            GlobalEvt::AllClientsDropped => {
                self.all_clients_dropped = true;
                send_msg(permit, MultiplexMsg::ClientFinish);
            }

            // Process that server has been dropped.
            GlobalEvt::ListenerDropped => {
                self.listen_tx = None;
                send_msg(permit, MultiplexMsg::ListenerFinish);
            }

            // Send Goodbye message.
            GlobalEvt::SendGoodbye => {
                self.goodbye_sent = true;
                send_msg(permit, MultiplexMsg::Goodbye);
            }
        }
        Ok(())
    }

    /// Handle message received from remote endpoint.
    #[tracing::instrument(level = "trace", skip_all, fields(msg=?received_msg.msg, data=?received_msg.data))]
    async fn handle_received_msg(
        &mut self, received_msg: TransportMsg,
    ) -> Result<(), ChMuxError<TransportSinkError, TransportStreamError>> {
        let TransportMsg { msg, data } = received_msg;

        match msg {
            // Connection reset by remote endpoint.
            MultiplexMsg::Reset => {
                return Err(ChMuxError::Reset);
            }

            // Hello message only allowed when establishing connection.
            MultiplexMsg::Hello { .. } => {
                return Err(protocol_err(
                    "received Hello message for already established multiplexer connection",
                ));
            }

            //  Nothing to do for ping message.
            MultiplexMsg::Ping => (),

            // Open port request from remote endpoint.
            MultiplexMsg::OpenPort { client_port, wait, id } => {
                if !self.outstanding_remote_port_requests.insert(client_port) {
                    return Err(protocol_err(format!(
                        "remote endpoint sent OpenPort request for same remote port {client_port} twice"
                    )));
                }
                let req = RemoteConnectMsg::Request(Request::new(
                    client_port,
                    id.unwrap_or(client_port),
                    wait,
                    self.port_allocator.clone(),
                    self.channel_tx.clone(),
                ));
                if let Some((listen_wait_tx, listen_no_wait_tx)) = &self.listen_tx {
                    let res = if wait { listen_wait_tx.try_send(req) } else { listen_no_wait_tx.try_send(req) };
                    if let Err(mpsc::error::TrySendError::Full(_)) = res {
                        return Err(protocol_err("remote endpoint sent too many OpenPort requests"));
                    }
                }
            }

            // Port opened response from remote endpoint.
            MultiplexMsg::PortOpened { client_port, server_port } => {
                match self.ports.remove_entry(&client_port) {
                    Some((local_port, PortState::Connecting { response_tx })) => {
                        let (sender, receiver) = self.create_port(local_port, server_port);
                        let _ = response_tx.send(ConnectResponse::Accepted(sender, receiver));
                    }
                    _ => {
                        return Err(protocol_err(format!(
                            "received PortOpened message for port {client_port} not in connecting state"
                        )));
                    }
                }
            }

            // Port open rejected response from remote endpoint.
            MultiplexMsg::Rejected { client_port, no_ports } => match self.ports.remove(&client_port) {
                Some(PortState::Connecting { response_tx }) => {
                    let _ = response_tx.send(ConnectResponse::Rejected { no_ports });
                }
                _ => {
                    return Err(protocol_err(format!(
                        "received Rejected message for port {client_port} not in connecting state"
                    )));
                }
            },

            // Data from remote endpoint.
            MultiplexMsg::Data { port, first, last } => {
                if let Some(PortState::Connected {
                    receiver_tx_data: Some(receiver_tx_data),
                    receiver_credit_monitor,
                    ..
                }) = self.ports.get_mut(&port)
                {
                    let data = data.unwrap();
                    let used_credit = match u32::try_from(data.len()) {
                        Ok(size) if size <= self.local_cfg.chunk_size => {
                            receiver_credit_monitor.use_credits(size.max(1))?
                        }
                        _ => {
                            return Err(protocol_err(format!(
                                "received data exceeds maximum chunk size on port {}",
                                &port
                            )));
                        }
                    };
                    let _ = receiver_tx_data.send(PortReceiveMsg::Data(ReceivedData {
                        buf: data,
                        first,
                        last,
                        credit: used_credit,
                    }));
                } else {
                    return Err(protocol_err(format!(
                        "received data for non-connected or finished local port {}",
                        &port
                    )));
                }
            }

            // Ports from remote endpoint.
            MultiplexMsg::PortData { port, first, last, wait, ports, ids } => {
                if let Some(PortState::Connected {
                    receiver_tx_data: Some(receiver_tx_data),
                    receiver_credit_monitor,
                    ..
                }) = self.ports.get_mut(&port)
                {
                    for port in &ports {
                        if !self.outstanding_remote_port_requests.insert(*port) {
                            return Err(protocol_err(format!(
                                "remote endpoint sent PortData request for same remote port {port} twice"
                            )));
                        }
                    }

                    let used_credit =
                        match ports.len().checked_mul(size_of::<u32>()).and_then(|v| u32::try_from(v).ok()) {
                            Some(size) if size <= self.local_cfg.chunk_size => {
                                receiver_credit_monitor.use_credits(size)?
                            }
                            _ => {
                                return Err(protocol_err(format!(
                                    "received ports exceeds maximum chunk size on port {}",
                                    &port
                                )));
                            }
                        };

                    let port_allocator = self.port_allocator.clone();
                    let channel_tx = self.channel_tx.clone();
                    let ids = ids.unwrap_or_else(|| ports.clone());
                    let requests = ports
                        .into_iter()
                        .zip(ids)
                        .map(|(remote_port, id)| {
                            Request::new(remote_port, id, wait, port_allocator.clone(), channel_tx.clone())
                        })
                        .collect();
                    let _ = receiver_tx_data.send(PortReceiveMsg::PortRequests(ReceivedPortRequests {
                        requests,
                        first,
                        last,
                        credit: used_credit,
                    }));
                } else {
                    return Err(protocol_err(format!(
                        "received port data for non-connected or finished local port {}",
                        &port
                    )));
                }
            }

            // Give flow credits to a port.
            MultiplexMsg::PortCredits { port, credits } => {
                if let Some(PortState::Connected { sender_credit_provider, .. }) = self.ports.get_mut(&port) {
                    sender_credit_provider.provide(credits)?;
                } else {
                    return Err(protocol_err(format!(
                        "received port credits message for port {} not in connected state",
                        &port
                    )));
                }
            }

            // Remote endpoint indicates that it will send no more data for port.
            MultiplexMsg::SendFinish { port } => {
                if let Some(PortState::Connected { receiver_tx_data, .. }) = self.ports.get_mut(&port) {
                    match receiver_tx_data.take() {
                        Some(receiver_tx_data) => {
                            let _ = receiver_tx_data.send(PortReceiveMsg::Finished);
                            self.maybe_free_port(port);
                        }
                        _ => {
                            return Err(protocol_err(format!(
                                "received SendFinish message for local port {} more than once",
                                &port
                            )));
                        }
                    }
                } else {
                    return Err(protocol_err(format!(
                        "received SendFinish message for local port {} not in connected state",
                        &port
                    )));
                }
            }

            // Remote indicates that the receiver for a port has been closed and it wishes
            // to receive no more data on that port.
            MultiplexMsg::ReceiveClose { port } => {
                if let Some(PortState::Connected {
                    sender_credit_provider,
                    remote_receiver_closed_notify,
                    remote_receiver_closed,
                    ..
                }) = self.ports.get_mut(&port)
                {
                    if !remote_receiver_closed.load(Ordering::Relaxed) {
                        // Disable credits provider.
                        sender_credit_provider.close(true);

                        // Send hangup notifications.
                        remote_receiver_closed.store(true, Ordering::Relaxed);
                        let notifies = remote_receiver_closed_notify.lock().unwrap().take().unwrap();
                        for tx in notifies {
                            let _ = tx.send(());
                        }

                        self.maybe_free_port(port);
                    } else {
                        return Err(protocol_err(format!(
                            "received more than one ReceiveClose message for port {}",
                            &port
                        )));
                    }
                } else {
                    return Err(protocol_err(format!(
                        "received ReceiveClose message for port {} not in connected state",
                        &port
                    )));
                }
            }

            // Remote hang up indicates that it will not process any more data on a port.
            MultiplexMsg::ReceiveFinish { port } => {
                if let Some(PortState::Connected {
                    sender_credit_provider,
                    remote_receiver_closed_notify,
                    remote_receiver_closed,
                    remote_receiver_dropped,
                    ..
                }) = self.ports.get_mut(&port)
                {
                    if !remote_receiver_closed.load(Ordering::Relaxed) {
                        // Disable credits provider.
                        sender_credit_provider.close(false);

                        // Send hangup notifications.
                        remote_receiver_closed.store(true, Ordering::Relaxed);
                        let notifies = remote_receiver_closed_notify.lock().unwrap().take().unwrap();
                        for tx in notifies {
                            let _ = tx.send(());
                        }
                    }

                    *remote_receiver_dropped = true;
                    self.maybe_free_port(port);
                } else {
                    return Err(protocol_err(format!(
                        "received ReceiveFinish message for port {} not in connected state",
                        &port
                    )));
                }
            }

            // Remote endpoint will send no more connect requests.
            MultiplexMsg::ClientFinish => {
                if let Some((listen_wait_tx, listen_no_wait_tx)) = &self.listen_tx {
                    // One additional slot is reserved in listen queue for sending the client
                    // dropped notification.
                    let mut failed = false;
                    if let Err(mpsc::error::TrySendError::Full(_)) =
                        listen_wait_tx.try_send(RemoteConnectMsg::ClientDropped)
                    {
                        failed = true;
                    }
                    if let Err(mpsc::error::TrySendError::Full(_)) =
                        listen_no_wait_tx.try_send(RemoteConnectMsg::ClientDropped)
                    {
                        failed = true;
                    }
                    if failed {
                        return Err(protocol_err(
                            "remote endpoint sent too many OpenPort or ClientFinish requests",
                        ));
                    }
                }

                self.remote_client_dropped = true;
            }

            // Remote endpoint will process no more connect requests.
            MultiplexMsg::ListenerFinish => {
                self.remote_listener_dropped.store(true, Ordering::Relaxed);
            }

            // Remote endpoint terminates connection.
            MultiplexMsg::Goodbye => {
                self.goodbye_received = true;
            }
        }

        Ok(())
    }
}

impl<TransportSink, TransportStream> Drop for ChMux<TransportSink, TransportStream> {
    fn drop(&mut self) {
        // Should be present to ensure correct drop order.
    }
}

/// A future that resolves when the sink is ready to receive a value.
struct SinkReady<S, Item> {
    sink: S,
    _item: PhantomData<Item>,
}

impl<S, Item> SinkReady<S, Item> {
    fn new(sink: S) -> Self {
        Self { sink, _item: PhantomData }
    }
}

impl<S, Item> Future for SinkReady<S, Item>
where
    S: Sink<Item> + Unpin,
    Item: Unpin,
{
    type Output = Result<(), S::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Pin::into_inner(self).sink.poll_ready_unpin(cx)
    }
}
