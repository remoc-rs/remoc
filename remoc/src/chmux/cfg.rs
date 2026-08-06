//! Channel multiplexer configuration.

use std::time::Duration;

use super::{
    msg::MAX_MSG_LENGTH,
    sizer::{BufferSizer, DynamicBuffer},
};

/// Behavior when ports are exhausted and a connect is requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PortsExhausted {
    /// Immediately fail connect request.
    Fail,
    /// Wait for a port to become available with an optional timeout.
    Wait(Option<Duration>),
}

/// Channel multiplexer configuration.
///
/// In most cases the default configuration ([Cfg::default]) is recommended, since it
/// provides a good balance between throughput, memory usage and latency.
///
/// In case of unsatisfactory performance (low throughput) your first step should be
/// to increase the [receive buffer size](Self::shared_receive_buffer).
#[derive(Debug, Clone)]
pub struct Cfg {
    /// Time after which the connection is closed when no data is received.
    ///
    /// Pings are send automatically when this is enabled and no data is transmitted.
    /// It also limits the duration of the initial handshake.
    ///
    /// By default this is 150 seconds, which is above the failure detection time of
    /// most transports; lower it to detect an unresponsive remote endpoint sooner.
    pub connection_timeout: Option<Duration>,
    /// Interval for flushing transport sink when more data is available to send.
    ///
    /// The transport sink is always flushed once there is no data readily available
    /// for sending.
    /// By default this is disabled (`None`).
    pub flush_interval: Option<Duration>,
    /// Buffer size for read and writes when [connecting over an IO transport](crate::Connect::io).
    ///
    /// By default this is 64 kB.
    pub io_buffer_size: usize,
    /// Maximum number of open ports.
    ///
    /// This must not exceed 2^30 = 1_073_741_824.
    /// By default this is 8192.
    pub max_ports: u32,
    /// Default behavior when ports are exhausted and a connect is requested.
    ///
    /// This can be overridden on a per-request basis.
    /// By default this is wait with a timeout of 60 seconds.
    pub ports_exhausted: PortsExhausted,
    /// Maximum size of received data per message in bytes.
    ///
    /// [Receiver::recv_chunk](super::Receiver::recv_chunk) is not affected by this limit.
    ///
    /// [Remote channels](crate::rch) will spawn a serialization and deserialization thread
    /// to transmit and receive data in chunks if this limit is reached.
    /// Thus, this does not limit the maximum serialized data size for remote channels
    /// but will incur a small performance cost for inter-thread communication when exceeded.
    ///
    /// This can be configured on a per-receiver basis.
    /// By default this is 512 kB.
    pub max_data_size: usize,
    /// Maximum port requests received per message.
    ///
    /// For [remote channels](crate::rch) this configures how many more ports than expected
    /// (from the data type) can be received per message.
    /// This is useful for compatibility when the receiver has an older version of a struct
    /// type with less fields containing ports.
    ///
    /// This can be configured on a per-receiver basis.
    /// By default this is 128.
    pub max_received_ports: usize,
    /// Size of a chunk of data in bytes.
    ///
    /// By default this is 32 kB.
    /// This must be at least 4 bytes.
    /// This must not exceed 2^32 - 16 = 4294967279.
    pub chunk_size: u32,
    /// Size of receive buffer of each port in bytes.
    ///
    /// This controls the maximum amout of in-flight data per port, that is data on the transport
    /// plus received but yet unprocessed data.
    ///
    /// By default this is 128 kB.
    /// This must be at least 4 bytes.
    pub port_receive_buffer: u32,
    /// Receive buffer level at which to throttle a port in bytes.
    ///
    /// By default this is 1 MB.
    pub port_receive_throttle: u32,
    /// Sizer for global receive buffer shared by all ports in bytes.
    ///
    /// Use a larger receive buffer if the throughput (bytes per second) is significantly
    /// lower than you would expect from your underlying transport connection.
    ///
    /// By default this is [dynamically adjusted](DynamicBuffer) with a minimum size
    /// of 64 kB and a maximum size of 128 MB.
    pub shared_receive_buffer: Box<dyn BufferSizer>,
    /// Length of global send queue.
    /// Each element holds a chunk.
    ///
    /// This limits the number of chunks sendable by using
    /// [Sender::try_send](super::Sender::try_send).
    /// It will not affect [remote channels](crate::rch).
    ///
    /// By default this is 32.
    /// This must not be zero.
    pub shared_send_queue: usize,
    /// Length of transport send queue.
    /// Each element holds a chunk.
    ///
    /// Raising this may improve performance but might incur a slight increase in latency.
    /// For minimum latency this should be set to 1.
    ///
    /// By default this is 32.
    /// This must not be zero.
    pub transport_send_queue: usize,
    /// Length of transport receive queue.
    /// Each element holds a chunk.
    ///
    /// Raising this may improve performance but might incur a slight increase in latency.
    /// For minimum latency this should be set to 1.
    ///
    /// By default this is 64.
    /// This must not be zero.
    pub transport_receive_queue: usize,
    /// Maximum number of outstanding connection requests.
    ///
    /// By default this is 128.
    /// This must not be zero.
    pub connect_queue: u16,
    /// Number of additional parallel transfer channels for the [mpsc channel](crate::rch::mpsc).
    ///
    /// Items are distributed over the channels in round-robin fashion, so that they can be
    /// serialized and deserialized concurrently. This pays off when serialization, rather
    /// than the link, limits throughput; otherwise it only spends additional CPU time.
    ///
    /// A value of 1 is not recommended, since it performs worse than using no additional
    /// channel at all. Use 2 or more; in our benchmarks 4 were enough to saturate a
    /// 100 MB/s link, but the value worth using depends on the payload, the codec and the
    /// machine.
    ///
    /// This can be overridden individually per channel using
    /// [`Sender::set_parallel`](crate::rch::mpsc::Sender::set_parallel)
    /// and
    /// [`Receiver::set_parallel`](crate::rch::mpsc::Receiver::set_parallel).
    ///
    /// By default this is 0, i.e. a channel transfers its items over a single channel.
    pub mpsc_parallel: usize,
    #[doc(hidden)]
    pub _non_exhaustive: (),
}

impl Default for Cfg {
    /// The default configuration provides a balance between throughput,
    /// memory usage and latency.
    fn default() -> Self {
        Self {
            connection_timeout: Some(Duration::from_secs(150)),
            flush_interval: None,
            io_buffer_size: 65_536,
            max_ports: 8192,
            ports_exhausted: PortsExhausted::Wait(Some(Duration::from_secs(60))),
            max_data_size: 524_288,
            max_received_ports: 128,
            chunk_size: 32_768,
            port_receive_buffer: 131_072,
            port_receive_throttle: 1_048_576,
            shared_receive_buffer: DynamicBuffer::new(65_536, 134_217_728),
            shared_send_queue: 32,
            transport_send_queue: 32,
            transport_receive_queue: 64,
            connect_queue: 128,
            mpsc_parallel: 0,
            _non_exhaustive: (),
        }
    }
}

impl Cfg {
    /// Checks the configuration.
    ///
    /// # Panics
    /// Panics if the configuration is invalid.
    pub(crate) fn check(&self) {
        if self.max_ports > 2u32.pow(30) {
            panic!("maximum ports must not exceed 2^30");
        }

        if self.chunk_size < 4 {
            panic!("chunk size must be at least 4");
        }

        if self.port_receive_buffer < 4 {
            panic!("port receive buffer must be at least 4 bytes");
        }

        if self.shared_send_queue == 0 {
            panic!("shared send queue length must not be zero");
        }

        if self.transport_send_queue == 0 {
            panic!("transport send queue length must not be zero");
        }

        if self.transport_receive_queue == 0 {
            panic!("transport receive queue length must not be zero");
        }

        if self.connect_queue == 0 {
            panic!("connect queue length must not be zero");
        }
    }

    /// Returns the maximum size of a frame that can be received by a
    /// channel multiplexer using this configuration.
    ///
    /// # Panics
    /// Panics if the configuration is invalid.
    pub fn max_frame_length(&self) -> u32 {
        (MAX_MSG_LENGTH as u32).checked_add(self.chunk_size).expect("maximum frame size exceeds u32::MAX")
    }
}
