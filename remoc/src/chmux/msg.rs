//! Multiplexer messages.

use byteorder::{LE, ReadBytesExt, WriteBytesExt};
use std::{
    io::{self, ErrorKind},
    time::Duration,
};

use super::{Cfg, ChMuxError, port_allocator::SidePort, sizer::GlobalCreditsReport};
use crate::varint::{VarintRead, VarintWrite};

fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("invalid value for {msg} received"))
}

/// Magic identifier.
pub const MAGIC: &[u8; 6] = b"CHMUX\0";

/// Message between two multiplexed endpoints.
#[derive(Debug)]
pub enum MultiplexMsg {
    /// Reset message.
    Reset,
    /// Hello message.
    Hello {
        // Magic identifier "CHMUX\0".
        /// Protocol version of side that sends the message.
        version: u8,
        /// Configuration of side that sends the message.
        cfg: ExchangedCfg,
    },
    /// Ping to keep connection alive when there is no data to send.
    Ping,
    /// Open connection on specified client port and assign a server port.
    OpenPort {
        /// Requesting client port.
        client_port: u32,
        // Flags u8.
        /// Wait for server port to become available.
        wait: bool,
        /// Port id
        id: Option<u32>,
        /// Pre-connect port.
        pre_connect: bool,
    },
    /// Connection accepted and server port assigned.
    PortOpened {
        /// Requesting client port.
        client_port: u32,
        /// Assigned server port.
        server_port: SidePort,
        // Flags u8.
    },
    /// Connection refused because server has no ports available.
    Rejected {
        /// Requesting client port.
        client_port: u32,
        // Flags u8.
        /// Rejected because no server ports was available and `wait` was not specified.
        no_ports: bool,
    },
    /// Data for specified port.
    ///
    /// This is followed by one data packet.
    Data {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
        /// First chunk of data.
        ///
        /// If there are chunks buffered at the moment, they are from a cancelled transmission
        /// and should be dropped.
        first: bool,
        /// Last chunk of data.
        last: bool,
        /// Credit source.
        credits: DataCredits,
    },
    /// Ports sent over a port.
    PortData {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
        /// First chunk of ports.
        ///
        /// If there are chunks buffered at the moment, they are from a cancelled transmission
        /// and should be dropped.
        first: bool,
        /// Last chunk of ports.
        last: bool,
        /// Wait for server port to become available.
        wait: bool,
        /// Sent ports.
        ports: Vec<PortDataItem>,
    },
    /// Request report when data has been processed up to this message.
    RequestReceivedReport {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
    },
    /// Reports that data has been processed up to this message.
    ReceivedReport {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
    },
    /// Give flow credits to a port.
    PortCredits {
        /// Port of side that receives this message.
        port: SidePort,
        /// Number of credits in bytes.
        credits: u32,
    },
    /// Port should stop using global credits.
    InhibitGlobalCreditUsageByPort {
        /// Port that should stop using global credits.
        port: SidePort,
    },
    /// Port should start using global credits again.
    AllowGlobalCreditUsageByPort {
        /// Port that should start using global credits.
        port: SidePort,
    },
    /// No more data will be sent to specified remote port.
    SendFinish {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
    },
    /// Not interested on receiving any more data from specified remote port,
    /// but already sent message will still be processed.
    ReceiveClose {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
    },
    /// No more messages for this port will be accepted.
    ReceiveFinish {
        /// Port of side that receives this message.
        port: SidePort,
        // Flags u8.
    },
    /// Give global credits, usable on any port.
    GlobalCredits(GlobalCredits),
    /// Report of held global credits.
    GlobalCreditsReport(GlobalCreditsReport),
    /// All clients have been dropped, therefore no more OpenPort requests will occur.
    ClientFinish,
    /// Listener has been dropped, therefore no more OpenPort requests will be handled.
    ListenerFinish,
    /// Terminate connection.
    Goodbye,
}

/// Port data item.
#[derive(Debug)]
pub struct PortDataItem {
    /// Port number.
    pub port: u32,
    /// Port id.
    pub id: u32,
    /// Pre connect port?
    pub pre_connect: bool,
}

pub const MSG_RESET: u8 = 1;
pub const MSG_HELLO: u8 = 2;
pub const MSG_PING: u8 = 3;
pub const MSG_OPEN_PORT: u8 = 4;
pub const MSG_PORT_OPENED: u8 = 5;
pub const MSG_REJECTED: u8 = 6;
pub const MSG_DATA: u8 = 7;
pub const MSG_PORT_DATA: u8 = 8;
pub const MSG_PORT_CREDITS: u8 = 9;
pub const MSG_SEND_FINISH: u8 = 10;
pub const MSG_RECEIVE_CLOSE: u8 = 11;
pub const MSG_RECEIVE_FINISH: u8 = 12;
pub const MSG_CLIENT_FINISH: u8 = 13;
pub const MSG_LISTENER_FINISH: u8 = 14;
pub const MSG_GOODBYE: u8 = 15;
pub const MSG_GLOBAL_CREDITS: u8 = 16;
pub const MSG_GLOBAL_CREDITS_REPORT: u8 = 17;
pub const MSG_INHIBIT_GLOBAL_CREDIT_USAGE_BY_PORT: u8 = 18;
pub const MSG_ALLOW_GLOBAL_CREDIT_USAGE_BY_PORT: u8 = 19;
pub const MSG_REQUEST_RECEIVED_REPORT: u8 = 20;
pub const MSG_RECEIVED_REPORT: u8 = 21;
pub const MSG_LOCAL_PORT_CREDITS: u8 = 22;
pub const MSG_INHIBIT_GLOBAL_CREDIT_USAGE_BY_LOCAL_PORT: u8 = 23;
pub const MSG_ALLOW_GLOBAL_CREDIT_USAGE_BY_LOCAL_PORT: u8 = 24;

pub const MSG_OPEN_PORT_FLAG_WAIT: u8 = 0b0000_0001;
pub const MSG_OPEN_PORT_FLAG_ID: u8 = 0b0000_0010;
pub const MSG_OPEN_PORT_FLAG_PRE_CONNECT: u8 = 0b0000_0100;

pub const MSG_PORT_OPENED_FLAG_SERVER_PORT_REMOTE: u8 = 0b0000_0001;

pub const MSG_REJECTED_FLAG_NO_PORTS: u8 = 0b0000_0001;

pub const MSG_DATA_FLAG_FIRST: u8 = 0b0000_0001;
pub const MSG_DATA_FLAG_LAST: u8 = 0b0000_0010;
pub const MSG_DATA_FLAG_CREDITS_GLOBAL: u8 = 0b0000_0100;
pub const MSG_DATA_FLAG_CREDITS_SPLIT: u8 = 0b0000_1000;
pub const MSG_DATA_FLAG_PORT_LOCAL: u8 = 0b0001_0000;

pub const MSG_PORT_DATA_FLAG_FIRST: u8 = 0b0000_0001;
pub const MSG_PORT_DATA_FLAG_LAST: u8 = 0b0000_0010;
pub const MSG_PORT_DATA_FLAG_WAIT: u8 = 0b0000_0100;
pub const MSG_PORT_DATA_FLAG_IDS: u8 = 0b0000_1000;
pub const MSG_PORT_DATA_FLAG_PORT_LOCAL: u8 = 0b0001_0000;
pub const MSG_PORT_DATA_FLAG_PORTS_FLAGS: u8 = 0b0010_0000;
pub const MSG_PORT_DATA_FLAG_PRE_CONNECT_ALL: u8 = 0b0100_0000;

pub const MSG_PORT_DATA_FLAG_PORT_FLAG_PRE_CONNECT: u8 = 0b0000_0001;

pub const MSG_REQUEST_RECEIVED_REPORT_FLAG_PORT_LOCAL: u8 = 0b0000_0001;

pub const MSG_RECEIVED_REPORT_FLAG_PORT_LOCAL: u8 = 0b0000_0001;

pub const MSG_SEND_FINISH_FLAG_PORT_LOCAL: u8 = 0b0000_0001;

pub const MSG_RECEIVE_CLOSE_FLAG_PORT_LOCAL: u8 = 0b0000_0001;

pub const MSG_RECEIVE_FINISH_FLAG_PORT_LOCAL: u8 = 0b0000_0001;

/// Maximum message length.
///
/// Currently this is 16 to reserve space for further use.
/// Port data, limited by the maximum chunk size, may be append to a message.
pub const MAX_MSG_LENGTH: usize = 16;

impl MultiplexMsg {
    pub(crate) fn write(&self, mut writer: impl io::Write, varint: bool) -> Result<(), io::Error> {
        match self {
            MultiplexMsg::Reset => {
                writer.write_u8(MSG_RESET)?;
            }
            MultiplexMsg::Hello { version, cfg } => {
                writer.write_u8(MSG_HELLO)?;
                writer.write_all(MAGIC)?;
                writer.write_u8(*version)?;
                cfg.write(&mut writer)?;
            }
            MultiplexMsg::Ping => {
                writer.write_u8(MSG_PING)?;
            }
            MultiplexMsg::OpenPort { client_port, wait, id, pre_connect } => {
                writer.write_u8(MSG_OPEN_PORT)?;
                writer.write_v32(*client_port, varint)?;
                let mut flags = 0;
                if *wait {
                    flags |= MSG_OPEN_PORT_FLAG_WAIT
                };
                if id.is_some() {
                    flags |= MSG_OPEN_PORT_FLAG_ID;
                }
                if *pre_connect {
                    flags |= MSG_OPEN_PORT_FLAG_PRE_CONNECT;
                }
                writer.write_u8(flags)?;
                if let Some(id) = id {
                    writer.write_v32(*id, varint)?;
                }
            }
            MultiplexMsg::PortOpened { client_port, server_port } => {
                writer.write_u8(MSG_PORT_OPENED)?;
                writer.write_v32(*client_port, varint)?;
                writer.write_v32(**server_port, varint)?;
                let mut flags = 0;
                if let SidePort::Remote(_) = server_port {
                    flags |= MSG_PORT_OPENED_FLAG_SERVER_PORT_REMOTE;
                }
                writer.write_u8(flags)?;
            }
            MultiplexMsg::Rejected { client_port, no_ports } => {
                writer.write_u8(MSG_REJECTED)?;
                writer.write_v32(*client_port, varint)?;
                writer.write_u8(if *no_ports { MSG_REJECTED_FLAG_NO_PORTS } else { 0 })?;
            }
            MultiplexMsg::Data { port, first, last, credits } => {
                writer.write_u8(MSG_DATA)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if *first {
                    flags |= MSG_DATA_FLAG_FIRST;
                }
                if *last {
                    flags |= MSG_DATA_FLAG_LAST;
                }
                match &credits {
                    DataCredits::PortOnly => (),
                    DataCredits::GlobalOnly => flags |= MSG_DATA_FLAG_CREDITS_GLOBAL,
                    DataCredits::GlobalAndPort(_) => flags |= MSG_DATA_FLAG_CREDITS_SPLIT,
                }
                if let SidePort::Local(_) = port {
                    flags |= MSG_DATA_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
                if let DataCredits::GlobalAndPort(global_credits) = credits {
                    writer.write_v32(*global_credits, varint)?;
                }
            }
            MultiplexMsg::PortData { port, first, last, wait, ports } => {
                writer.write_u8(MSG_PORT_DATA)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if *first {
                    flags |= MSG_PORT_DATA_FLAG_FIRST;
                }
                if *last {
                    flags |= MSG_PORT_DATA_FLAG_LAST;
                }
                if *wait {
                    flags |= MSG_PORT_DATA_FLAG_WAIT;
                }
                let need_ids = ports.iter().any(|port| port.port != port.id);
                if need_ids {
                    flags |= MSG_PORT_DATA_FLAG_IDS;
                }
                let pre_connect_count = ports.iter().filter(|port| port.pre_connect).count();
                let need_port_flags = if pre_connect_count == 0 {
                    false
                } else if ports.iter().any(|port| port.port != port.id) {
                    flags |= MSG_PORT_DATA_FLAG_PRE_CONNECT_ALL;
                    false
                } else {
                    flags |= MSG_PORT_DATA_FLAG_PORTS_FLAGS;
                    true
                };
                if let SidePort::Local(_) = port {
                    flags |= MSG_PORT_DATA_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
                for port in ports {
                    writer.write_v32(port.port, varint)?;
                    if need_ids {
                        writer.write_v32(port.id, varint)?;
                    }
                    if need_port_flags {
                        let mut port_flags = 0;
                        if port.pre_connect {
                            port_flags |= MSG_PORT_DATA_FLAG_PORT_FLAG_PRE_CONNECT;
                        }
                        writer.write_u8(port_flags)?;
                    }
                }
            }
            MultiplexMsg::RequestReceivedReport { port } => {
                writer.write_u8(MSG_REQUEST_RECEIVED_REPORT)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if let SidePort::Local(_) = port {
                    flags |= MSG_REQUEST_RECEIVED_REPORT_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
            }
            MultiplexMsg::ReceivedReport { port } => {
                writer.write_u8(MSG_RECEIVED_REPORT)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if let SidePort::Local(_) = port {
                    flags |= MSG_RECEIVED_REPORT_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
            }
            MultiplexMsg::PortCredits { port, credits } => {
                match port {
                    SidePort::Remote(_) => writer.write_u8(MSG_PORT_CREDITS)?,
                    SidePort::Local(_) => writer.write_u8(MSG_LOCAL_PORT_CREDITS)?,
                }
                writer.write_v32(**port, varint)?;
                writer.write_v32(*credits, varint)?;
            }
            MultiplexMsg::InhibitGlobalCreditUsageByPort { port } => {
                match port {
                    SidePort::Remote(_) => writer.write_u8(MSG_INHIBIT_GLOBAL_CREDIT_USAGE_BY_PORT)?,
                    SidePort::Local(_) => writer.write_u8(MSG_INHIBIT_GLOBAL_CREDIT_USAGE_BY_LOCAL_PORT)?,
                }
                writer.write_v32(**port, varint)?;
            }
            MultiplexMsg::AllowGlobalCreditUsageByPort { port } => {
                match port {
                    SidePort::Remote(_) => writer.write_u8(MSG_ALLOW_GLOBAL_CREDIT_USAGE_BY_PORT)?,
                    SidePort::Local(_) => writer.write_u8(MSG_ALLOW_GLOBAL_CREDIT_USAGE_BY_LOCAL_PORT)?,
                }
                writer.write_v32(**port, varint)?;
            }
            MultiplexMsg::SendFinish { port } => {
                writer.write_u8(MSG_SEND_FINISH)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if let SidePort::Local(_) = port {
                    flags |= MSG_SEND_FINISH_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
            }
            MultiplexMsg::ReceiveClose { port } => {
                writer.write_u8(MSG_RECEIVE_CLOSE)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if let SidePort::Local(_) = port {
                    flags |= MSG_RECEIVE_CLOSE_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
            }
            MultiplexMsg::ReceiveFinish { port } => {
                writer.write_u8(MSG_RECEIVE_FINISH)?;
                writer.write_v32(**port, varint)?;
                let mut flags = 0;
                if let SidePort::Local(_) = port {
                    flags |= MSG_RECEIVE_FINISH_FLAG_PORT_LOCAL;
                }
                writer.write_u8(flags)?;
            }
            MultiplexMsg::GlobalCredits(GlobalCredits { credits, seq }) => {
                writer.write_u8(MSG_GLOBAL_CREDITS)?;
                writer.write_v32(*credits, varint)?;
                writer.write_u8(*seq)?;
            }
            MultiplexMsg::GlobalCreditsReport(GlobalCreditsReport { current, min, seq }) => {
                writer.write_u8(MSG_GLOBAL_CREDITS_REPORT)?;
                writer.write_v32(*current, varint)?;
                writer.write_v32(*min, varint)?;
                writer.write_u8(*seq)?;
            }
            MultiplexMsg::ClientFinish => {
                writer.write_u8(MSG_CLIENT_FINISH)?;
            }
            MultiplexMsg::ListenerFinish => {
                writer.write_u8(MSG_LISTENER_FINISH)?;
            }
            MultiplexMsg::Goodbye => {
                writer.write_u8(MSG_GOODBYE)?;
            }
        }
        Ok(())
    }

    pub(crate) fn read(mut reader: impl io::Read, varint: bool) -> Result<Self, io::Error> {
        let msg = match reader.read_u8()? {
            MSG_RESET => Self::Reset,
            MSG_HELLO => {
                let mut magic = vec![0; MAGIC.len()];
                reader.read_exact(&mut magic)?;
                if magic != MAGIC {
                    return Err(invalid_data("invalid magic"));
                }
                Self::Hello { version: reader.read_u8()?, cfg: ExchangedCfg::read(&mut reader)? }
            }
            MSG_PING => Self::Ping,
            MSG_OPEN_PORT => {
                let client_port = reader.read_v32(varint)?;
                let flags = reader.read_u8()?;
                let wait = flags & MSG_OPEN_PORT_FLAG_WAIT != 0;
                let mut id = (flags & MSG_OPEN_PORT_FLAG_ID != 0).then_some(0);
                let pre_open = flags & MSG_OPEN_PORT_FLAG_PRE_CONNECT != 0;
                if let Some(id) = &mut id {
                    *id = reader.read_v32(varint)?;
                }
                Self::OpenPort { client_port, wait, id, pre_connect: pre_open }
            }
            MSG_PORT_OPENED => {
                let client_port = reader.read_v32(varint)?;
                let server_port = reader.read_v32(varint)?;
                let flags = reader.read_u8().unwrap_or_default();
                Self::PortOpened {
                    client_port,
                    server_port: if flags & MSG_PORT_OPENED_FLAG_SERVER_PORT_REMOTE != 0 {
                        SidePort::Remote(server_port)
                    } else {
                        SidePort::Local(server_port)
                    },
                }
            }
            MSG_REJECTED => Self::Rejected {
                client_port: reader.read_v32(varint)?,
                no_ports: reader.read_u8()? & MSG_REJECTED_FLAG_NO_PORTS != 0,
            },
            MSG_DATA => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8()?;
                let credits = if flags & MSG_DATA_FLAG_CREDITS_GLOBAL != 0 {
                    DataCredits::GlobalOnly
                } else if flags & MSG_DATA_FLAG_CREDITS_SPLIT != 0 {
                    DataCredits::GlobalAndPort(reader.read_v32(varint)?)
                } else {
                    DataCredits::PortOnly
                };
                Self::Data {
                    port: if flags & MSG_DATA_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                    first: flags & MSG_DATA_FLAG_FIRST != 0,
                    last: flags & MSG_DATA_FLAG_LAST != 0,
                    credits,
                }
            }
            MSG_PORT_DATA => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8()?;
                let first = flags & MSG_PORT_DATA_FLAG_FIRST != 0;
                let last = flags & MSG_PORT_DATA_FLAG_LAST != 0;
                let wait = flags & MSG_PORT_DATA_FLAG_WAIT != 0;
                let mut ports = Vec::with_capacity(16);
                loop {
                    let port = match reader.read_v32(varint) {
                        Ok(port) => port,
                        Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
                        Err(err) => return Err(err),
                    };
                    let id = if flags & MSG_PORT_DATA_FLAG_IDS != 0 { reader.read_v32(varint)? } else { port };
                    let mut pre_connect = false;
                    if flags & MSG_PORT_DATA_FLAG_PORTS_FLAGS != 0 {
                        let port_flags = reader.read_u8()?;
                        pre_connect = port_flags & MSG_PORT_DATA_FLAG_PORT_FLAG_PRE_CONNECT != 0;
                    }
                    ports.push(PortDataItem { port, id, pre_connect });
                }
                Self::PortData {
                    port: if flags & MSG_PORT_DATA_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                    first,
                    last,
                    wait,
                    ports,
                }
            }
            MSG_REQUEST_RECEIVED_REPORT => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8().unwrap_or_default();
                Self::RequestReceivedReport {
                    port: if flags & MSG_REQUEST_RECEIVED_REPORT_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                }
            }
            MSG_RECEIVED_REPORT => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8().unwrap_or_default();
                Self::ReceivedReport {
                    port: if flags & MSG_RECEIVED_REPORT_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                }
            }
            MSG_PORT_CREDITS => Self::PortCredits {
                port: SidePort::Remote(reader.read_v32(varint)?),
                credits: reader.read_v32(varint)?,
            },
            MSG_LOCAL_PORT_CREDITS => Self::PortCredits {
                port: SidePort::Local(reader.read_v32(varint)?),
                credits: reader.read_v32(varint)?,
            },
            MSG_INHIBIT_GLOBAL_CREDIT_USAGE_BY_PORT => {
                Self::InhibitGlobalCreditUsageByPort { port: SidePort::Remote(reader.read_v32(varint)?) }
            }
            MSG_INHIBIT_GLOBAL_CREDIT_USAGE_BY_LOCAL_PORT => {
                Self::InhibitGlobalCreditUsageByPort { port: SidePort::Local(reader.read_v32(varint)?) }
            }
            MSG_ALLOW_GLOBAL_CREDIT_USAGE_BY_PORT => {
                Self::AllowGlobalCreditUsageByPort { port: SidePort::Remote(reader.read_v32(varint)?) }
            }
            MSG_ALLOW_GLOBAL_CREDIT_USAGE_BY_LOCAL_PORT => {
                Self::AllowGlobalCreditUsageByPort { port: SidePort::Local(reader.read_v32(varint)?) }
            }
            MSG_SEND_FINISH => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8().unwrap_or_default();
                Self::SendFinish {
                    port: if flags & MSG_SEND_FINISH_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                }
            }
            MSG_RECEIVE_CLOSE => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8().unwrap_or_default();
                Self::ReceiveClose {
                    port: if flags & MSG_RECEIVE_CLOSE_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                }
            }
            MSG_RECEIVE_FINISH => {
                let port = reader.read_v32(varint)?;
                let flags = reader.read_u8().unwrap_or_default();
                Self::ReceiveFinish {
                    port: if flags & MSG_RECEIVE_FINISH_FLAG_PORT_LOCAL != 0 {
                        SidePort::Local(port)
                    } else {
                        SidePort::Remote(port)
                    },
                }
            }
            MSG_GLOBAL_CREDITS => {
                Self::GlobalCredits(GlobalCredits { credits: reader.read_v32(varint)?, seq: reader.read_u8()? })
            }
            MSG_GLOBAL_CREDITS_REPORT => Self::GlobalCreditsReport(GlobalCreditsReport {
                current: reader.read_v32(varint)?,
                min: reader.read_v32(varint)?,
                seq: reader.read_u8()?,
            }),
            MSG_CLIENT_FINISH => Self::ClientFinish,
            MSG_LISTENER_FINISH => Self::ListenerFinish,
            MSG_GOODBYE => Self::Goodbye,
            _ => return Err(invalid_data("invalid message id")),
        };
        Ok(msg)
    }

    pub(crate) fn to_vec(&self, varint: bool) -> Vec<u8> {
        let mut data = Vec::with_capacity(MAX_MSG_LENGTH);
        self.write(&mut data, varint).expect("message serialization failed");
        data
    }

    pub(crate) fn from_slice<SinkError, StreamError>(
        data: &[u8], varint: bool,
    ) -> Result<Self, ChMuxError<SinkError, StreamError>> {
        Self::read(data, varint).map_err(|err| ChMuxError::Protocol(err.to_string()))
    }
}

/// Multiplexer configuration exchanged with remote endpoint.
#[derive(Clone, Debug)]
pub struct ExchangedCfg {
    /// Time after which connection is closed when no data is.
    pub connection_timeout: Option<Duration>,
    /// Size of a chunk of data in bytes.
    pub chunk_size: u32,
    /// Size of receive buffer of each port in bytes.
    pub port_receive_buffer: u32,
    /// Length of connection request queue.
    pub connect_queue: u16,
    /// Initial global credits, if supported.
    pub global_credits: Option<u32>,
    /// Whether sending received reports is supported.
    pub received_report: bool,
    /// Whether port side specification is supported.
    pub port_side: bool,
    /// Whether pre-connecting ports is supported.
    pub pre_connect: bool,
    /// Variable integer encoding is supported.
    pub varint: bool,
    /// Message frame length variable integer encoding is supported.
    pub frame_len_varint: bool,
    /// Transported types use compact serialized format.
    pub compact_transported: bool,
}

impl ExchangedCfg {
    /// Create exchanged configuration.
    pub fn new(cfg: &Cfg, global_credits: u32) -> Self {
        Self {
            connection_timeout: cfg.connection_timeout,
            chunk_size: cfg.chunk_size,
            port_receive_buffer: cfg.port_receive_buffer,
            connect_queue: cfg.connect_queue,
            global_credits: Some(global_credits),
            received_report: true,
            port_side: true,
            pre_connect: true,
            varint: true,
            frame_len_varint: cfg.io_frame_len_varint.is_some(),
            compact_transported: true,
        }
    }

    pub(crate) fn write(&self, mut writer: impl io::Write) -> Result<(), io::Error> {
        writer.write_u64::<LE>(
            self.connection_timeout.unwrap_or_default().as_millis().min(u64::MAX as u128) as u64
        )?;
        writer.write_u32::<LE>(self.chunk_size)?;
        writer.write_u32::<LE>(self.port_receive_buffer)?;
        writer.write_u16::<LE>(self.connect_queue)?;
        writer.write_u32::<LE>(self.global_credits.unwrap())?;
        writer.write_u8(self.received_report.into())?;
        writer.write_u8(self.port_side.into())?;
        writer.write_u8(self.pre_connect.into())?;
        writer.write_u8(self.varint.into())?;
        writer.write_u8(self.frame_len_varint.into())?;
        writer.write_u8(self.compact_transported.into())?;

        Ok(())
    }

    pub(crate) fn read(mut reader: impl io::Read) -> Result<Self, io::Error> {
        let mut this = Self {
            connection_timeout: match reader.read_u64::<LE>()? {
                0 => None,
                millis => Some(Duration::from_millis(millis)),
            },
            chunk_size: match reader.read_u32::<LE>()? {
                cs if cs >= 4 => cs,
                _ => return Err(invalid_data("chunk_size")),
            },
            port_receive_buffer: match reader.read_u32::<LE>()? {
                prb if prb >= 4 => prb,
                _ => return Err(invalid_data("port_receive_buffer")),
            },
            connect_queue: match reader.read_u16::<LE>()? {
                cq if cq >= 1 => cq,
                _ => return Err(invalid_data("connect_queue must not be zero")),
            },
            global_credits: None,
            received_report: false,
            port_side: false,
            pre_connect: false,
            varint: false,
            frame_len_varint: false,
            compact_transported: false,
        };

        let Ok(global_credits) = reader.read_u32::<LE>() else { return Ok(this) };
        this.global_credits = Some(global_credits);

        let Ok(received_report) = reader.read_u8() else { return Ok(this) };
        this.received_report = received_report != 0;

        let Ok(port_side) = reader.read_u8() else { return Ok(this) };
        this.port_side = port_side != 0;

        let Ok(pre_connect) = reader.read_u8() else { return Ok(this) };
        this.pre_connect = pre_connect != 0;

        let Ok(varint) = reader.read_u8() else { return Ok(this) };
        this.varint = varint != 0;

        let Ok(frame_len_varint) = reader.read_u8() else { return Ok(this) };
        this.frame_len_varint = frame_len_varint != 0;

        let Ok(compact_transported) = reader.read_u8() else { return Ok(this) };
        this.compact_transported = compact_transported != 0;

        Ok(this)
    }
}

/// Credits used for data.
#[derive(Default, Debug, Clone)]
#[must_use]
pub enum DataCredits {
    /// Use only port credits.
    #[default]
    PortOnly,
    /// Use only global credits.
    GlobalOnly,
    /// Use then specified number of global credits and rest port credits.
    GlobalAndPort(u32),
}

/// Provided global credits.
#[derive(Debug, Clone)]
pub struct GlobalCredits {
    /// Number of credits in bytes.
    pub credits: u32,
    /// Sequence number in [GlobalCreditsStatus::seq] for credit status reporting.
    pub seq: u8,
}
