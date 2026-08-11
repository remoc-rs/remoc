use std::{
    borrow::Borrow,
    collections::{BTreeSet, HashSet, VecDeque},
    error::Error,
    fmt,
    hash::Hash,
    io::ErrorKind,
    ops::Deref,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use super::{cfg::OnPortsExhausted, msg::PortDataItem};
use crate::exec::time::timeout;

/// The requested remote port number has alredy been allocated.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(super) struct RemotePortAlreadyAllocated(pub u32);

impl fmt::Display for RemotePortAlreadyAllocated {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "remote port {} is already allocated", self.0)
    }
}

impl Error for RemotePortAlreadyAllocated {}

impl From<RemotePortAlreadyAllocated> for std::io::Error {
    fn from(err: RemotePortAlreadyAllocated) -> Self {
        Self::new(ErrorKind::AddrInUse, err)
    }
}

/// No ports or connection requests are currently available.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PortsExhausted;

impl fmt::Display for PortsExhausted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "no ports or connection requests are currently available")
    }
}

impl Error for PortsExhausted {}

impl From<PortsExhausted> for std::io::Error {
    fn from(err: PortsExhausted) -> Self {
        Self::new(ErrorKind::AddrInUse, err)
    }
}

/// Try running the waiting and trying variants according to the ports exhausted policy.
async fn try_exhausted<T>(
    on_ports_exhausted: &OnPortsExhausted, wait_fn: impl AsyncFnOnce() -> T, try_fn: impl FnOnce() -> Option<T>,
) -> Result<T, PortsExhausted> {
    match on_ports_exhausted {
        OnPortsExhausted::Fail => try_fn().ok_or(PortsExhausted),
        OnPortsExhausted::Wait => Ok(wait_fn().await),
        OnPortsExhausted::Timeout(duration) => timeout(*duration, wait_fn()).await.map_err(|_| PortsExhausted),
    }
}

struct PortAllocatorInner {
    local_used: HashSet<u32>,
    local_pre_allocated: HashSet<u32>,
    remote_used: HashSet<u32>,
    reserved: usize,
    quarantined_set: BTreeSet<u32>,
    quarantined_list: VecDeque<u32>,
    limit: usize,
    next: u32,
    ports_exhausted: OnPortsExhausted,
    notify: Arc<Notify>,
}

impl PortAllocatorInner {
    const PRE_ALLOCATED_LIMIT: usize = 131_072;

    fn is_available(&self) -> bool {
        self.local_used.len() + self.remote_used.len() + self.reserved < self.limit
    }

    fn try_allocate_pre_allocated(
        &mut self, this: &Arc<Mutex<PortAllocatorInner>>, number: u32,
    ) -> Option<AllocatedLocalPort> {
        if self.is_available() && !self.local_used.contains(&number) && !self.quarantined_set.contains(&number) {
            self.local_used.insert(number);
            assert!(self.local_pre_allocated.remove(&number));
            Some(AllocatedLocalPort { number, allocator: this.clone() })
        } else {
            None
        }
    }

    fn try_allocate_local(&mut self, this: &Arc<Mutex<PortAllocatorInner>>) -> Option<AllocatedLocalPort> {
        if !self.is_available() {
            return None;
        }

        let number = loop {
            self.next = self.next.wrapping_add(1);
            if self.is_available()
                && !self.local_used.contains(&self.next)
                && !self.local_pre_allocated.contains(&self.next)
                && !self.quarantined_set.contains(&self.next)
            {
                break self.next;
            }
        };

        self.local_used.insert(number);
        Some(AllocatedLocalPort { number, allocator: this.clone() })
    }

    fn try_pre_allocate_local(&mut self, this: &Arc<Mutex<PortAllocatorInner>>) -> Option<PreAllocatedLocalPort> {
        if self.local_pre_allocated.len() >= Self::PRE_ALLOCATED_LIMIT {
            return None;
        }

        let number = loop {
            self.next = self.next.wrapping_add(1);
            if !self.local_used.contains(&self.next)
                && !self.local_pre_allocated.contains(&self.next)
                && !self.quarantined_set.contains(&self.next)
            {
                break self.next;
            }
        };

        self.local_pre_allocated.insert(number);
        Some(PreAllocatedLocalPort { number, allocator: this.clone(), ports_exhausted: self.ports_exhausted })
    }

    fn try_allocate_remote(
        &mut self, this: Arc<Mutex<PortAllocatorInner>>, number: u32,
    ) -> Result<Option<AllocatedRemotePort>, RemotePortAlreadyAllocated> {
        if !self.is_available() {
            return Ok(None);
        }

        if self.remote_used.insert(number) {
            Ok(Some(AllocatedRemotePort { number, allocator: this }))
        } else {
            Err(RemotePortAlreadyAllocated(number))
        }
    }

    fn try_reserve(&mut self, this: Arc<Mutex<PortAllocatorInner>>) -> Option<ReservedPort> {
        if !self.is_available() {
            return None;
        }

        self.reserved += 1;
        Some(ReservedPort { allocator: this, used: false })
    }
}

/// Local port number allocator.
///
/// State is shared between clones of this type.
#[derive(Clone)]
pub struct PortAllocator {
    ports: Arc<Mutex<PortAllocatorInner>>,
    connect_credits: Arc<Semaphore>,
    port_side_supported: bool,
    pre_connect_supported: bool,
    ports_exhausted: OnPortsExhausted,
}

impl fmt::Debug for PortAllocator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let ports = self.ports.lock().unwrap();
        f.debug_struct("PortAllocator")
            .field("local_used", &ports.local_used.len())
            .field("local_pre_allocated", &ports.local_pre_allocated.len())
            .field("remote_used", &ports.remote_used.len())
            .field("reserved", &ports.reserved)
            .field("port_limit", &ports.limit)
            .field("connect_credits", &self.connect_credits.available_permits())
            .field("port_side_supported", &self.port_side_supported)
            .field("pre_connect_supported", &self.pre_connect_supported)
            .field("ports_exhausted", &self.ports_exhausted)
            .finish()
    }
}

impl PortAllocator {
    /// Creates a new port number allocator.
    pub(crate) fn new(
        port_limit: u32, connect_limit: u16, port_side_supported: bool, pre_connect_supported: bool,
        ports_exhausted: OnPortsExhausted,
    ) -> PortAllocator {
        let ports = PortAllocatorInner {
            local_used: HashSet::new(),
            local_pre_allocated: HashSet::new(),
            remote_used: HashSet::new(),
            reserved: 0,
            quarantined_set: BTreeSet::new(),
            quarantined_list: VecDeque::new(),
            limit: port_limit as usize,
            next: 0,
            ports_exhausted,
            notify: Arc::new(Notify::new()),
        };

        Self {
            ports: Arc::new(Mutex::new(ports)),
            connect_credits: Arc::new(Semaphore::new(connect_limit.into())),
            port_side_supported,
            pre_connect_supported,
            ports_exhausted,
        }
    }

    /// Whether the remote endpoint supports port side specification.
    pub(super) fn is_port_side_supported(&self) -> bool {
        self.port_side_supported
    }

    /// Whether pre-connecting ports is supported by the remote endpoint.
    pub(super) fn is_pre_connect_supported(&self) -> bool {
        self.pre_connect_supported
    }

    /// Pre-allocates a local port number.
    ///
    /// If the pre-allocation limit is exceeded, `None` is returned.
    pub(super) fn pre_allocate_local(&self) -> Result<PreAllocatedLocalPort, PortsExhausted> {
        let mut ports = self.ports.lock().unwrap();
        ports.try_pre_allocate_local(&self.ports).ok_or(PortsExhausted)
    }

    /// Reserves a port number for accepting an incoming connection.
    ///
    /// If all ports are currently in use, this waits for a port number to become available
    /// according to the configuration.
    pub(super) async fn reserve(&self) -> Result<ReservedPort, PortsExhausted> {
        try_exhausted(&self.ports_exhausted, async || self.wait_reserve().await, || self.try_reserve()).await
    }

    /// Reserves a port number for accepting an incoming connection.
    ///
    /// If all ports are currently in use, this waits for a port number to become available
    /// forever.    
    pub(super) async fn wait_reserve(&self) -> ReservedPort {
        loop {
            let notified;

            {
                let mut inner = self.ports.lock().unwrap();
                notified = inner.notify.clone().notified_owned();

                if let Some(reserved) = inner.try_reserve(self.ports.clone()) {
                    return reserved;
                }
            }

            notified.await;
        }
    }

    /// Tries to reserve a port number for accepting an incoming connection.
    ///
    /// If all ports are currently in use, this returns [None].
    pub(super) fn try_reserve(&self) -> Option<ReservedPort> {
        let mut ports = self.ports.lock().unwrap();
        ports.try_reserve(self.ports.clone())
    }

    /// Obtains a connection request credit.
    ///
    /// Waits for the credit to become available according to the configuration.
    pub(super) async fn connect_req_credit(&self) -> Result<ConnectReqCredit, PortsExhausted> {
        try_exhausted(
            &self.ports_exhausted,
            async || self.wait_connect_req_credit().await,
            || self.try_connect_req_credit(),
        )
        .await
    }

    /// Obtains a connection request credit.
    ///
    /// Waits for the credit to become available forever.
    async fn wait_connect_req_credit(&self) -> ConnectReqCredit {
        ConnectReqCredit(self.connect_credits.clone().acquire_owned().await.unwrap())
    }

    /// Tries to obtain a connection request credit.
    ///
    /// Does not wait for the credit to become available.
    pub(super) fn try_connect_req_credit(&self) -> Option<ConnectReqCredit> {
        self.connect_credits.clone().try_acquire_owned().ok().map(ConnectReqCredit)
    }

    /// Allocates a port connection request.
    pub fn connect_req(&self) -> Result<ConnectReq, PortsExhausted> {
        ConnectReq::new(self.clone())
    }
}

/// A reservation for an allocated local or allocated remote port number.
pub(super) struct ReservedPort {
    allocator: Arc<Mutex<PortAllocatorInner>>,
    used: bool,
}

impl fmt::Debug for ReservedPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ReservedPort").finish()
    }
}

impl ReservedPort {
    /// Turns this reservation into an allocated local port.
    pub fn into_local(mut self) -> AllocatedLocalPort {
        let mut inner = self.allocator.lock().unwrap();
        inner.reserved -= 1;
        self.used = true;
        inner.try_allocate_local(&self.allocator).unwrap()
    }

    /// Turns this reservation into an allocated local port.
    pub fn into_remote(mut self, remote_port: u32) -> Result<AllocatedRemotePort, RemotePortAlreadyAllocated> {
        let mut inner = self.allocator.lock().unwrap();
        inner.reserved -= 1;
        self.used = true;
        inner.try_allocate_remote(self.allocator.clone(), remote_port).transpose().unwrap()
    }
}

impl Drop for ReservedPort {
    fn drop(&mut self) {
        if self.used {
            return;
        }

        let mut inner = self.allocator.lock().unwrap();
        inner.reserved -= 1;
        inner.notify.notify_waiters();
    }
}

/// An allocated local port number.
///
/// When this is dropped, the allocation is automatically released.
pub(super) struct AllocatedLocalPort {
    number: u32,
    allocator: Arc<Mutex<PortAllocatorInner>>,
}

impl fmt::Debug for AllocatedLocalPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl fmt::Display for AllocatedLocalPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl Deref for AllocatedLocalPort {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.number
    }
}

impl PartialEq for AllocatedLocalPort {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for AllocatedLocalPort {}

impl PartialOrd for AllocatedLocalPort {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AllocatedLocalPort {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.number.cmp(&other.number)
    }
}

impl Hash for AllocatedLocalPort {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl Borrow<u32> for AllocatedLocalPort {
    fn borrow(&self) -> &u32 {
        &self.number
    }
}

impl Drop for AllocatedLocalPort {
    fn drop(&mut self) {
        let mut inner = self.allocator.lock().unwrap();

        inner.local_used.remove(&self.number);

        inner.quarantined_set.insert(self.number);
        inner.quarantined_list.push_back(self.number);
        while inner.quarantined_list.len() > inner.limit + 1 {
            let port = inner.quarantined_list.pop_front().unwrap();
            inner.quarantined_set.remove(&port);
        }

        inner.notify.notify_waiters();
    }
}

/// An pre-allocated local port number.
///
/// When this is dropped, the pre-allocation is automatically released.
pub(super) struct PreAllocatedLocalPort {
    number: u32,
    allocator: Arc<Mutex<PortAllocatorInner>>,
    ports_exhausted: OnPortsExhausted,
}

impl PreAllocatedLocalPort {
    /// Waits until the port becomes available according to the configuration and allocates it.
    pub async fn allocate(self) -> Result<AllocatedLocalPort, PortsExhausted> {
        match &self.ports_exhausted {
            OnPortsExhausted::Fail => self.try_allocate().map_err(|_| PortsExhausted),
            OnPortsExhausted::Wait => Ok(self.wait_allocate().await),
            OnPortsExhausted::Timeout(duration) => {
                timeout(*duration, self.wait_allocate()).await.map_err(|_| PortsExhausted)
            }
        }
    }

    /// Waits until the port becomes available forever and allocates it.
    async fn wait_allocate(self) -> AllocatedLocalPort {
        loop {
            let notified;

            {
                let mut inner = self.allocator.lock().unwrap();
                notified = inner.notify.clone().notified_owned();

                if let Some(number) = inner.try_allocate_pre_allocated(&self.allocator, self.number) {
                    return number;
                }
            }

            notified.await;
        }
    }

    /// Allocates the port, if it is readily available.
    pub fn try_allocate(self) -> Result<AllocatedLocalPort, PreAllocatedLocalPort> {
        let mut inner = self.allocator.lock().unwrap();
        match inner.try_allocate_pre_allocated(&self.allocator, self.number) {
            Some(allocated) => Ok(allocated),
            None => {
                drop(inner);
                Err(self)
            }
        }
    }
}

impl fmt::Debug for PreAllocatedLocalPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl fmt::Display for PreAllocatedLocalPort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl Deref for PreAllocatedLocalPort {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.number
    }
}

impl PartialEq for PreAllocatedLocalPort {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for PreAllocatedLocalPort {}

impl PartialOrd for PreAllocatedLocalPort {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PreAllocatedLocalPort {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.number.cmp(&other.number)
    }
}

impl Hash for PreAllocatedLocalPort {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl Borrow<u32> for PreAllocatedLocalPort {
    fn borrow(&self) -> &u32 {
        &self.number
    }
}

impl Drop for PreAllocatedLocalPort {
    fn drop(&mut self) {
        let mut inner = self.allocator.lock().unwrap();
        inner.local_pre_allocated.remove(&self.number);
    }
}

/// An allocated remote port number.
///
/// When this is dropped, the allocation is automatically released.
pub(super) struct AllocatedRemotePort {
    number: u32,
    allocator: Arc<Mutex<PortAllocatorInner>>,
}

impl fmt::Debug for AllocatedRemotePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl fmt::Display for AllocatedRemotePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl Deref for AllocatedRemotePort {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.number
    }
}

impl PartialEq for AllocatedRemotePort {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for AllocatedRemotePort {}

impl PartialOrd for AllocatedRemotePort {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AllocatedRemotePort {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.number.cmp(&other.number)
    }
}

impl Hash for AllocatedRemotePort {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl Borrow<u32> for AllocatedRemotePort {
    fn borrow(&self) -> &u32 {
        &self.number
    }
}

impl Drop for AllocatedRemotePort {
    fn drop(&mut self) {
        let mut inner = self.allocator.lock().unwrap();
        inner.remote_used.remove(&self.number);
        inner.notify.notify_waiters();
    }
}

/// Allocated local or allocated remote port number.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum AllocatedSidePort {
    /// Allocated local port number.
    Local(AllocatedLocalPort),
    /// Allocated remote port number.
    Remote(AllocatedRemotePort),
}

impl From<AllocatedLocalPort> for AllocatedSidePort {
    fn from(port: AllocatedLocalPort) -> Self {
        Self::Local(port)
    }
}

impl From<AllocatedRemotePort> for AllocatedSidePort {
    fn from(port: AllocatedRemotePort) -> Self {
        Self::Remote(port)
    }
}

impl AllocatedSidePort {
    /// Local or remote port number.
    pub fn side_port(&self) -> SidePort {
        SidePort::from(self)
    }
}

impl fmt::Debug for AllocatedSidePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", SidePort::from(self))
    }
}

impl fmt::Display for AllocatedSidePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", SidePort::from(self))
    }
}

/// Local or remote port number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SidePort {
    /// Local port number.
    Local(u32),
    /// Remote port number.
    Remote(u32),
}

impl SidePort {
    /// Whether this is a local port number.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Whether this is a remote port number.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// Flips the port side in-place.
    pub(super) fn flip(&mut self) {
        *self = self.flipped();
    }

    /// Returns the flipped port side.
    #[must_use]
    pub(super) fn flipped(&self) -> Self {
        match self {
            Self::Local(port) => Self::Remote(*port),
            Self::Remote(port) => Self::Local(*port),
        }
    }
}

impl fmt::Debug for SidePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for SidePort {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Local(port) => write!(f, "L{port}"),
            Self::Remote(port) => write!(f, "R{port}"),
        }
    }
}

impl Deref for SidePort {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Local(num) => num,
            Self::Remote(num) => num,
        }
    }
}

impl From<&AllocatedSidePort> for SidePort {
    fn from(port: &AllocatedSidePort) -> Self {
        match port {
            AllocatedSidePort::Local(port) => Self::Local(**port),
            AllocatedSidePort::Remote(port) => Self::Remote(**port),
        }
    }
}

/// A credit for requesting a connection.
#[expect(unused)]
pub(super) struct ConnectReqCredit(OwnedSemaphorePermit);

impl fmt::Debug for ConnectReqCredit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_tuple("ConnectRequestCredit").finish()
    }
}

/// Internal port request.
#[derive(Debug)]
pub(super) struct PortReq {
    /// Allocated local port number.
    pub port: AllocatedLocalPort,
    /// Options.
    pub opts: PortReqOpts,
}

/// Port requests options.
#[derive(Debug)]
pub(super) struct PortReqOpts {
    /// A user-specified id.
    pub id: u32,
    /// Wait for remote port to become available?
    pub wait: bool,
    /// If pre-connect, connection request credits.
    pub pre_connect_credit: Option<ConnectReqCredit>,
}

impl From<&PortReq> for PortDataItem {
    fn from(req: &PortReq) -> Self {
        PortDataItem { port: *req.port, id: req.opts.id, pre_connect: req.opts.pre_connect_credit.is_some() }
    }
}

/// A port connection request by the local endpoint.
///
/// The [id](Self::with_id) can be set freely by the user.
/// It is initialized to the [port number](Self::port).
pub struct ConnectReq {
    /// Port allocator.
    port_allocator: PortAllocator,
    /// Pre-allocated local port.    
    pre_port: PreAllocatedLocalPort,
    /// Port options.
    opts: PortReqOpts,
}

impl fmt::Debug for ConnectReq {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ConnectReq")
            .field("port", &self.port())
            .field("id", &self.opts.id)
            .field("wait", &self.opts.wait)
            .field("is_pre_connected", &self.is_pre_connected())
            .finish()
    }
}

impl ConnectReq {
    fn new(port_allocator: PortAllocator) -> Result<Self, PortsExhausted> {
        let pre_port = port_allocator.pre_allocate_local()?;
        Ok(Self {
            port_allocator,
            opts: PortReqOpts { id: *pre_port, wait: true, pre_connect_credit: None },
            pre_port,
        })
    }

    pub(super) async fn into_port_req(self) -> Result<PortReq, PortsExhausted> {
        let Self { pre_port, opts, .. } = self;
        let port = if opts.wait {
            pre_port.allocate().await?
        } else {
            pre_port.try_allocate().map_err(|_| PortsExhausted)?
        };
        Ok(PortReq { port, opts })
    }

    /// Allocated port number.
    pub fn port(&self) -> u32 {
        *self.pre_port
    }

    /// Sets the id to the specified value.
    #[must_use]
    pub fn with_id(mut self, id: u32) -> Self {
        self.opts.id = id;
        self
    }

    /// Do not wait for a local and remote ports to become available.
    ///
    /// If this is called and the local or remote endpoint has no ports readily available,
    /// the connect request will fail regardless of the setting of
    /// [`Cfg::ports_exhausted`](`super::cfg::Cfg::ports_exhausted`).
    #[must_use]
    pub fn no_wait(mut self) -> Self {
        self.opts.wait = false;
        self
    }

    /// Pre-connects the port before the remote endpoint accepts the connection request.
    ///
    /// This allows sending data immediately over the port, even before the remote endpoint
    /// replies to the connection request, thus saving the round-trip time between the local and
    /// remote endpoint.    
    ///
    /// If the remote endpoint does not support pre-connection, this is ignored.
    ///
    /// This waits until a slot is available in the connect queue according to the
    /// [configuration](super::cfg::Cfg::ports_exhausted). If no slot becomes available,
    /// the port is not pre-connected.
    #[must_use]
    pub async fn pre_connect(mut self) -> Self {
        if self.port_allocator.is_pre_connect_supported() && self.opts.pre_connect_credit.is_none() {
            self.opts.pre_connect_credit = self.port_allocator.connect_req_credit().await.ok();
        }
        self
    }

    /// Pre-connects the port before the remote endpoint accepts the connection request,
    /// if a slot in the connect queue is readily available.
    ///
    /// If no slot is available in the connect queue, the port is not pre-connected.
    ///
    /// See [pre_connect](Self::pre_connect) for details.
    #[must_use]
    pub fn try_pre_connect(mut self) -> Self {
        if self.port_allocator.is_pre_connect_supported() && self.opts.pre_connect_credit.is_none() {
            self.opts.pre_connect_credit = self.port_allocator.try_connect_req_credit();
        }
        self
    }

    /// Whether the port will be pre-connected.
    ///
    /// See [pre_connect](Self::pre_connect) for details.
    pub fn is_pre_connected(&self) -> bool {
        self.opts.pre_connect_credit.is_some()
    }
}
