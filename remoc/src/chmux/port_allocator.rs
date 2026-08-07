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
use tokio::sync::Notify;

/// The requested remote port number has alredy been allocated.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RemotePortAlreadyAllocated(pub u32);

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

struct PortAllocatorInner {
    local_used: HashSet<u32>,
    remote_used: HashSet<u32>,
    reserved: usize,
    quarantined_set: BTreeSet<u32>,
    quarantined_list: VecDeque<u32>,
    limit: usize,
    next: u32,
    notify: Arc<Notify>,
}

impl PortAllocatorInner {
    fn is_available(&self) -> bool {
        self.local_used.len() + self.remote_used.len() + self.reserved < self.limit
    }

    fn try_allocate_local(&mut self, this: Arc<Mutex<PortAllocatorInner>>) -> Option<AllocatedLocalPort> {
        if !self.is_available() {
            return None;
        }

        let number = loop {
            self.next = self.next.wrapping_add(1);
            if !self.local_used.contains(&self.next) && !self.quarantined_set.contains(&self.next) {
                break self.next;
            }
        };

        self.local_used.insert(number);
        Some(AllocatedLocalPort { number, allocator: this })
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
    inner: Arc<Mutex<PortAllocatorInner>>,
    port_side_supported: bool,
}

impl fmt::Debug for PortAllocator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = self.inner.lock().unwrap();
        f.debug_struct("PortAllocator")
            .field("local_used", &inner.local_used.len())
            .field("remote_used", &inner.remote_used.len())
            .field("reserved", &inner.reserved)
            .field("limit", &inner.limit)
            .field("port_side_supported", &self.port_side_supported)
            .finish()
    }
}

impl PortAllocator {
    /// Creates a new port number allocator.
    pub(crate) fn new(limit: u32, port_side_supported: bool) -> PortAllocator {
        let inner = PortAllocatorInner {
            local_used: HashSet::new(),
            remote_used: HashSet::new(),
            reserved: 0,
            quarantined_set: BTreeSet::new(),
            quarantined_list: VecDeque::new(),
            limit: limit as usize,
            next: 0,
            notify: Arc::new(Notify::new()),
        };
        PortAllocator { inner: Arc::new(Mutex::new(inner)), port_side_supported }
    }

    /// Whether the remote endpoint supports port side specification.
    pub fn is_port_side_supported(&self) -> bool {
        self.port_side_supported
    }

    /// Allocates a local port number.
    ///
    /// Port numbers are allocated sequentially.
    /// If all ports are currently in use, this waits for a port number to become available.
    pub async fn allocate_local(&self) -> AllocatedLocalPort {
        loop {
            let notified;

            {
                let mut inner = self.inner.lock().unwrap();
                notified = inner.notify.clone().notified_owned();

                if let Some(number) = inner.try_allocate_local(self.inner.clone()) {
                    return number;
                }
            }

            notified.await;
        }
    }

    /// Tries to allocate a local port number.
    ///
    /// If all ports are currently in use, this returns [None].
    pub fn try_allocate_local(&self) -> Option<AllocatedLocalPort> {
        let mut inner = self.inner.lock().unwrap();
        inner.try_allocate_local(self.inner.clone())
    }

    /// Allocates the specified remote port number.
    ///
    /// If all ports are currently in use, this waits for a port number to become available.
    pub async fn allocate_remote(
        &self, remote_port: u32,
    ) -> Result<AllocatedRemotePort, RemotePortAlreadyAllocated> {
        loop {
            let notified;

            {
                let mut inner = self.inner.lock().unwrap();
                notified = inner.notify.clone().notified_owned();

                if let Some(allocated) = inner.try_allocate_remote(self.inner.clone(), remote_port)? {
                    return Ok(allocated);
                }
            }

            notified.await;
        }
    }

    /// Tries to allocate the specified remote port number.
    ///
    /// If all ports are currently in use, this returns [None].
    pub fn try_allocate_remote(
        &self, remote_port: u32,
    ) -> Result<Option<AllocatedRemotePort>, RemotePortAlreadyAllocated> {
        let mut inner = self.inner.lock().unwrap();
        inner.try_allocate_remote(self.inner.clone(), remote_port)
    }

    /// Reserves a port number.
    ///
    /// If all ports are currently in use, this waits for a port number to become available.
    pub async fn reserve(&self) -> ReservedPort {
        loop {
            let notified;

            {
                let mut inner = self.inner.lock().unwrap();
                notified = inner.notify.clone().notified_owned();

                if let Some(reserved) = inner.try_reserve(self.inner.clone()) {
                    return reserved;
                }
            }

            notified.await;
        }
    }

    /// Tries to reserve a local or remote port number.
    ///
    /// If all ports are currently in use, this returns [None].
    pub fn try_reserve(&self) -> Option<ReservedPort> {
        let mut inner = self.inner.lock().unwrap();
        inner.try_reserve(self.inner.clone())
    }
}

/// A reservation for an allocated local or allocated remote port number.
pub struct ReservedPort {
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
        inner.try_allocate_local(self.allocator.clone()).unwrap()
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
/// When this is dropped, the allocated is automatically released.
pub struct AllocatedLocalPort {
    number: u32,
    allocator: Arc<Mutex<PortAllocatorInner>>,
}

impl AllocatedLocalPort {
    /// Local port number.
    pub fn num(&self) -> u32 {
        self.number
    }
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

/// An allocated remote port number.
///
/// When this is dropped, the allocated is automatically released.
pub struct AllocatedRemotePort {
    number: u32,
    allocator: Arc<Mutex<PortAllocatorInner>>,
}

impl AllocatedRemotePort {
    /// Remote port number.
    pub fn num(&self) -> u32 {
        self.number
    }
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
pub enum AllocatedSidePort {
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
    /// Whether this is an allocated local port number.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Whether this is an allocated remote port number.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

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
    pub fn flip(&mut self) {
        *self = self.flipped();
    }

    /// Returns the flipped port side.
    #[must_use]
    pub fn flipped(&self) -> Self {
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

/// A port connection request by the local endpoint.
///
/// The id can be set freely by the user.
/// It is initialized to the [port number](Self::port).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortReq {
    /// The allocated, local port number.
    pub port: AllocatedLocalPort,
    /// A user-specified id.
    pub id: u32,
}

impl From<AllocatedLocalPort> for PortReq {
    /// Create a new port connection request with [`id`](Self::id) set to
    /// the [port number](Self::port).
    fn from(port: AllocatedLocalPort) -> Self {
        Self { id: port.number, port }
    }
}

impl From<PortReq> for AllocatedLocalPort {
    fn from(req: PortReq) -> Self {
        req.port
    }
}

impl PortReq {
    /// Create a new port connection request with [`id`](Self::id) set to
    /// the [port number](Self::port).
    pub fn new(port: AllocatedLocalPort) -> Self {
        Self::from(port)
    }

    /// Sets the id to the specified value.
    pub fn with_id(mut self, id: u32) -> Self {
        self.id = id;
        self
    }
}
