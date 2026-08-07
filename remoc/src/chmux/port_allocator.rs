use std::{
    borrow::Borrow,
    collections::{BTreeSet, HashSet, VecDeque},
    fmt,
    hash::Hash,
    ops::Deref,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

struct PortAllocatorInner {
    used: HashSet<u32>,
    quarantined_set: BTreeSet<u32>,
    quarantined_list: VecDeque<u32>,
    limit: usize,
    next: u32,
    notify: Arc<Notify>,
}

impl PortAllocatorInner {
    fn is_available(&self) -> bool {
        self.used.len() < self.limit
    }

    fn try_allocate(&mut self, this: Arc<Mutex<PortAllocatorInner>>) -> Option<PortNumber> {
        if !self.is_available() {
            return None;
        }

        let number = loop {
            self.next = self.next.wrapping_add(1);
            if !self.used.contains(&self.next) && !self.quarantined_set.contains(&self.next) {
                break self.next;
            }
        };

        self.used.insert(number);
        Some(PortNumber { number, allocator: this })
    }
}

/// Local port number allocator.
///
/// State is shared between clones of this type.
#[derive(Clone)]
pub struct PortAllocator(Arc<Mutex<PortAllocatorInner>>);

impl fmt::Debug for PortAllocator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = self.0.lock().unwrap();
        f.debug_struct("PortAllocator").field("used", &inner.used.len()).field("limit", &inner.limit).finish()
    }
}

impl PortAllocator {
    /// Creates a new port number allocator.
    pub(crate) fn new(limit: u32) -> PortAllocator {
        let inner = PortAllocatorInner {
            used: HashSet::new(),
            quarantined_set: BTreeSet::new(),
            quarantined_list: VecDeque::new(),
            limit: limit as usize,
            next: 0,
            notify: Arc::new(Notify::new()),
        };
        PortAllocator(Arc::new(Mutex::new(inner)))
    }

    /// Allocates a local port number.
    ///
    /// Port numbers are allocated sequentially.
    /// If all ports are currently in use, this waits for a port number to become available.
    pub async fn allocate(&self) -> PortNumber {
        loop {
            let notified;

            {
                let mut inner = self.0.lock().unwrap();
                notified = inner.notify.clone().notified_owned();

                if let Some(number) = inner.try_allocate(self.0.clone()) {
                    return number;
                }
            }

            notified.await;
        }
    }

    /// Tries to allocate a local port number.
    ///
    /// If all port are currently in use, this returns [None].
    pub fn try_allocate(&self) -> Option<PortNumber> {
        let mut inner = self.0.lock().unwrap();
        inner.try_allocate(self.0.clone())
    }
}

/// An allocated local port number.
///
/// When this is dropped, the allocated is automatically released.
pub struct PortNumber {
    number: u32,
    allocator: Arc<Mutex<PortAllocatorInner>>,
}

impl PortNumber {
    /// Local port number.
    pub fn num(&self) -> u32 {
        self.number
    }
}

impl fmt::Debug for PortNumber {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl fmt::Display for PortNumber {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.number)
    }
}

impl Deref for PortNumber {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.number
    }
}

impl PartialEq for PortNumber {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for PortNumber {}

impl PartialOrd for PortNumber {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PortNumber {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.number.cmp(&other.number)
    }
}

impl Hash for PortNumber {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl Borrow<u32> for PortNumber {
    fn borrow(&self) -> &u32 {
        &self.number
    }
}

impl Drop for PortNumber {
    fn drop(&mut self) {
        let mut inner = self.allocator.lock().unwrap();

        inner.used.remove(&self.number);

        inner.quarantined_set.insert(self.number);
        inner.quarantined_list.push_back(self.number);
        while inner.quarantined_list.len() > inner.limit + 1 {
            let port = inner.quarantined_list.pop_front().unwrap();
            inner.quarantined_set.remove(&port);
        }

        inner.notify.notify_waiters();
    }
}

/// Allocated local or remote port number.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SidePortNumber {
    /// Allocated local port number.
    Local(PortNumber),
    /// Remote port number.
    Remote(u32),
}

impl SidePortNumber {
    /// Local or remote port number.
    pub fn num(&self) -> SidePort {
        SidePort::from(self)
    }
}

impl fmt::Debug for SidePortNumber {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", SidePort::from(self))
    }
}

impl fmt::Display for SidePortNumber {
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

impl From<&SidePortNumber> for SidePort {
    fn from(port: &SidePortNumber) -> Self {
        match port {
            SidePortNumber::Local(port) => Self::Local(**port),
            SidePortNumber::Remote(port) => Self::Remote(*port),
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
    pub port: PortNumber,
    /// A user-specified id.
    pub id: u32,
}

impl From<PortNumber> for PortReq {
    /// Create a new port connection request with [`id`](Self::id) set to
    /// the [port number](Self::port).
    fn from(port: PortNumber) -> Self {
        Self { id: port.number, port }
    }
}

impl From<PortReq> for PortNumber {
    fn from(req: PortReq) -> Self {
        req.port
    }
}

impl PortReq {
    /// Create a new port connection request with [`id`](Self::id) set to
    /// the [port number](Self::port).
    pub fn new(port: PortNumber) -> Self {
        Self::from(port)
    }

    /// Sets the id to the specified value.
    pub fn with_id(mut self, id: u32) -> Self {
        self.id = id;
        self
    }
}
