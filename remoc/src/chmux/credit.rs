//! Flow credit management.

use futures::{FutureExt, future::BoxFuture};
use std::{
    collections::VecDeque,
    fmt, mem,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
};
use tokio::sync::{
    Notify,
    mpsc::{self, error::TrySendError},
};

use super::{
    ChMuxError, SendError,
    msg::{DataCredits, GlobalCredits},
    mux::PortEvt,
    sizer::{BufferSize, BufferSizeQuery, BufferSizer, GlobalCreditsReport},
};

// ===========================================================================
// Credit accounting for sending data
// ===========================================================================

/// Credit pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreditPool {
    /// Port credits.
    Port,
    /// Global credits.
    Global,
}

/// Assigned credits.
///
/// The credits can be used to send data.
/// Unused dropped credits are returned to the the credit providers.
#[derive(Debug)]
pub(crate) struct AssignedCredits {
    pool: CreditPool,
    credits: u32,
    inner: Weak<SendableCreditStore>,
}

impl AssignedCredits {
    /// Empty assigned credits.
    pub fn empty(pool: CreditPool) -> Self {
        Self { pool, credits: 0, inner: Weak::new() }
    }

    /// Create with specified number of credits.
    fn new(credits: u32, inner: &Arc<SendableCreditStore>) -> Self {
        Self { pool: inner.pool, credits, inner: Arc::downgrade(inner) }
    }

    /// True if no credits are contained.
    pub fn is_empty(&self) -> bool {
        self.credits == 0
    }

    /// Available credits.
    pub fn available(&self) -> u32 {
        self.credits
    }

    /// Takes credits out for sending data.
    ///
    /// Panics when insufficient credits are available.
    pub fn take(&mut self, credits: u32) {
        if self.credits < credits {
            panic!("insufficient {:?} AssignedCredits", self.pool);
        }

        self.credits -= credits;
    }
}

impl Drop for AssignedCredits {
    fn drop(&mut self) {
        if self.credits > 0
            && let Some(inner) = self.inner.upgrade()
        {
            inner.credits.fetch_add(self.credits, Ordering::Relaxed);
            inner.notify.notify_waiters();
        }
    }
}

/// Assigned credits, possibly mixed from port and global credits.
#[derive(Default, Debug)]
pub(crate) struct MixedAssignedCredits(VecDeque<AssignedCredits>);

impl MixedAssignedCredits {
    /// Empty assigned credits.
    pub fn new() -> Self {
        Self::default()
    }

    /// True if no credits are contained.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Available credits.
    pub fn available(&self) -> u32 {
        self.0.iter().map(|ac| ac.available()).sum()
    }

    /// Add assigned credits.
    pub fn add(&mut self, ac: AssignedCredits) {
        assert!(!ac.is_empty());
        self.0.push_back(ac);
    }

    /// Takes credits out for sending data.
    ///
    /// Panics when insufficient credits are available.
    pub fn take(&mut self, mut credits: u32) -> DataCredits {
        let mut global = 0;
        let mut port = 0;

        while credits > 0 {
            let ac = self.0.front_mut().expect("insufficient MergedAssignedCredits");
            let take = ac.available().min(credits);

            ac.take(take);
            credits -= take;

            match ac.pool {
                CreditPool::Global => global += take,
                CreditPool::Port => port += take,
            }

            if ac.is_empty() {
                self.0.pop_front().unwrap();
            }
        }

        if global == 0 {
            DataCredits::PortOnly
        } else if port == 0 {
            DataCredits::GlobalOnly
        } else {
            DataCredits::GlobalAndPort(global)
        }
    }
}

/// Holds sendable credits.
#[derive(Debug)]
struct SendableCreditStore {
    /// Credit pool.
    pool: CreditPool,
    /// Number of credits we currently hold for sending.
    credits: AtomicU32,
    /// Channel state.
    state: AtomicU8,
    /// Whether global credits may be used.
    use_global_credits: AtomicBool,
    /// Notify that is signaled if anything changes.
    notify: Notify,
}

impl SendableCreditStore {
    const STATE_OPEN: u8 = 0;
    const STATE_CLOSED: u8 = 1;
    const STATE_CLOSED_GRACEFULLY: u8 = 2;

    /// Creates a new instance with the specified number of initial credits.
    fn new(pool: CreditPool, credits: u32) -> Self {
        Self {
            pool,
            credits: credits.into(),
            state: Self::STATE_OPEN.into(),
            use_global_credits: true.into(),
            notify: Notify::new(),
        }
    }

    /// Check that sending over channel is possible.
    fn check_sendable(&self, override_graceful_close: bool) -> Result<(), SendError> {
        if self.pool != CreditPool::Port {
            return Ok(());
        }

        match self.state.load(Ordering::Relaxed) {
            Self::STATE_OPEN => Ok(()),
            Self::STATE_CLOSED => Err(SendError::Closed { gracefully: false }),
            Self::STATE_CLOSED_GRACEFULLY => {
                if override_graceful_close {
                    Ok(())
                } else {
                    Err(SendError::Closed { gracefully: true })
                }
            }
            _ => unreachable!("invalid channel state"),
        }
    }

    /// Take up to `req` credits, but a minimum of `min_req` credits.
    /// If insufficient, zero is returned.
    fn try_take_credits(&self, req: u32, min_req: u32) -> u32 {
        let mut taken = 0;
        let _ = self.credits.try_update(Ordering::Relaxed, Ordering::Relaxed, |credits| {
            if credits >= min_req {
                taken = credits.min(req);
                Some(credits - taken)
            } else {
                taken = 0;
                None
            }
        });
        taken
    }
}

/// Provides credits for sending data.
#[derive(Debug)]
pub(crate) struct CreditProvider {
    inner: Arc<SendableCreditStore>,
    min: u32,
    seq: u8,
}

impl CreditProvider {
    /// Creates a new instance.
    fn new(inner: Arc<SendableCreditStore>) -> Self {
        Self { min: inner.credits.load(Ordering::Relaxed), seq: 0, inner }
    }

    /// Provides the given count of credits for consumption.
    pub fn provide<SinkError, StreamError>(
        &mut self, credits: u32,
    ) -> Result<(), ChMuxError<SinkError, StreamError>> {
        let old_credits = self.inner.credits.fetch_add(credits, Ordering::Relaxed);
        if old_credits.checked_add(credits).is_none() {
            return Err(ChMuxError::Protocol("credits overflow".to_string()));
        }

        self.min = self.min.min(old_credits);
        self.inner.notify.notify_waiters();

        Ok(())
    }

    /// Sets whether global credits should be used by a port.
    pub fn set_use_global_credits(&mut self, use_global_credits: bool) {
        self.inner.use_global_credits.store(use_global_credits, Ordering::Relaxed);
        self.inner.notify.notify_waiters();
    }

    /// Closes the channel.
    pub fn close(&self, gracefully: bool) {
        let state = if gracefully {
            SendableCreditStore::STATE_CLOSED_GRACEFULLY
        } else {
            SendableCreditStore::STATE_CLOSED
        };
        self.inner.state.store(state, Ordering::Relaxed);
        self.inner.notify.notify_waiters();
    }

    /// Returns the current status.
    ///
    /// This reset the minimum credit statistic.
    /// When the sequence number is changed, the minimum credits statistic is reset.
    pub fn take_status(&mut self, new_seq: u8) -> GlobalCreditsReport {
        let current = self.inner.credits.load(Ordering::Relaxed);

        if self.seq != new_seq {
            self.seq = new_seq;
            self.min = current;
        }

        let report = GlobalCreditsReport { current, min: self.min.min(current), seq: self.seq };

        self.min = current;

        report
    }
}

impl Drop for CreditProvider {
    fn drop(&mut self) {
        if self.inner.state.load(Ordering::Relaxed) == SendableCreditStore::STATE_OPEN {
            self.close(false);
        }
    }
}

/// Requests and consumes credits for sending over a channel.
pub(crate) struct CreditUser {
    inner: Weak<SendableCreditStore>,
    /// Whether data is sent anyway, when remote endpoint closed channel gracefully.
    pub override_graceful_close: bool,
}

impl CreditUser {
    fn new(inner: Weak<SendableCreditStore>) -> Self {
        Self { inner, override_graceful_close: false }
    }

    /// Check that data is sendable over this channel.
    pub fn check_sendable(&self) -> Result<(), SendError> {
        let Some(inner) = self.inner.upgrade() else { return Err(SendError::ChMux) };
        inner.check_sendable(self.override_graceful_close)
    }

    /// Whether remote endpoint allows global credit use.
    pub fn remote_allows_global_credits(&self) -> bool {
        let Some(inner) = self.inner.upgrade() else { return true };
        inner.use_global_credits.load(Ordering::Relaxed)
    }

    /// Waits for global credit usage to be allowed by remote endpoint.
    pub async fn wait_for_remote_allowing_global_credits(&self) {
        let Some(inner) = self.inner.upgrade() else { return };

        loop {
            let notify = inner.notify.notified();
            if inner.check_sendable(self.override_graceful_close).is_err() {
                return;
            }

            if inner.use_global_credits.load(Ordering::Relaxed) {
                break;
            }

            notify.await;
        }
    }

    /// Requests credits for sending.
    /// Blocks until at least `min_req` credits become available.
    pub async fn request(&self, req: u32, min_req: u32) -> Result<AssignedCredits, SendError> {
        debug_assert!(req > 0);
        debug_assert!(req >= min_req);

        let Some(inner) = self.inner.upgrade() else { return Err(SendError::ChMux) };

        loop {
            let notified = inner.notify.notified();
            inner.check_sendable(self.override_graceful_close)?;

            match inner.try_take_credits(req, min_req) {
                0 => {
                    tracing::trace!(
                        "waiting for at least {min_req} credits, but want {req} {:?} credits",
                        inner.pool
                    );
                    notified.await;
                }
                taken => {
                    tracing::trace!("obtained {taken} of {req} {:?} requested credits", inner.pool);
                    return Ok(AssignedCredits::new(taken, &inner));
                }
            }
        }
    }

    /// Requests the specified number of credits for sending without blocking.
    /// Returns credits up to `req` if at least `min_req` available, otherwise None.
    pub fn try_request(&self, req: u32, min_req: u32) -> Result<Option<AssignedCredits>, SendError> {
        debug_assert!(req > 0);
        debug_assert!(req >= min_req);

        let Some(inner) = self.inner.upgrade() else { return Err(SendError::ChMux) };

        inner.check_sendable(self.override_graceful_close)?;

        match inner.try_take_credits(req, min_req) {
            0 => Ok(None),
            taken => {
                tracing::trace!("obtained {taken} of {req} {:?} requested credits", inner.pool);
                Ok(Some(AssignedCredits::new(taken, &inner)))
            }
        }
    }
}

/// Creates a pair of credit provider and credit user, initially filled
/// with the specified number of credits.
pub(crate) fn credit_send_pair(pool: CreditPool, initial_credits: u32) -> (CreditProvider, CreditUser) {
    let inner = Arc::new(SendableCreditStore::new(pool, initial_credits));

    let user = CreditUser::new(Arc::downgrade(&inner));
    let provider = CreditProvider::new(inner);
    (provider, user)
}

// ===========================================================================
// Credit accounting and return scheduling for received data
// ===========================================================================

/// Represents monitored used port credits.
#[derive(Default, Debug)]
pub(crate) struct UsedPortCredit {
    port: u32,
    global: u32,
}

impl fmt::Display for UsedPortCredit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}+{}", self.port, self.global)
    }
}

#[derive(Debug)]
struct PortCreditMonitorInner {
    port_used: AtomicU32,
    port_limit: u32,
    throttle: u32,
    global_used: AtomicU32,
    global_credit_usage: AtomicU8,
}

impl PortCreditMonitorInner {
    const GLOBAL_CREDIT_USAGE_ACTIVE: u8 = 0;
    const GLOBAL_CREDIT_USAGE_INHIBITING: u8 = 1;
    const GLOBAL_CREDIT_USAGE_INHIBITED: u8 = 2;
}

/// Monitors port-specific credits.
#[derive(Debug)]
pub(crate) struct PortCreditMonitor(Arc<PortCreditMonitorInner>);

impl PortCreditMonitor {
    /// Use port-specific credits.
    pub fn use_credits<SinkError, StreamError>(
        &self, port_credits: u32, global_credits: u32,
    ) -> Result<UsedPortCredit, ChMuxError<SinkError, StreamError>> {
        let Some(new_port_used) =
            self.0.port_used.fetch_add(port_credits, Ordering::Relaxed).checked_add(port_credits)
        else {
            return Err(ChMuxError::Protocol("remote endpoint overflowed used port flow credits".to_string()));
        };

        if new_port_used > self.0.port_limit {
            return Err(ChMuxError::Protocol(format!(
                "remote endpoint tried to use {new_port_used} port flow credits but only {} are available",
                self.0.port_limit
            )));
        }

        let Some(new_global_used) =
            self.0.global_used.fetch_add(global_credits, Ordering::Relaxed).checked_add(global_credits)
        else {
            return Err(ChMuxError::Protocol("remote endpoint overflowed used global flow credits".to_string()));
        };

        let total_used = new_port_used.saturating_add(new_global_used);
        if total_used >= self.0.throttle {
            let _ = self.0.global_credit_usage.compare_exchange(
                PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_ACTIVE,
                PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_INHIBITING,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }

        Ok(UsedPortCredit { port: port_credits, global: global_credits })
    }

    /// Whether the credit monitor currently requests the remote endpoint to stop using global credits on this port.
    ///
    /// If `will_inhibit` is true, the request will be marked as executed.
    pub fn inhibiting_global_credit_usage(&self, will_inhibit: bool) -> bool {
        if will_inhibit {
            self.0
                .global_credit_usage
                .compare_exchange(
                    PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_INHIBITING,
                    PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_INHIBITED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        } else {
            self.0.global_credit_usage.load(Ordering::Relaxed)
                == PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_INHIBITING
        }
    }
}

/// Queues prot credits for return to the sending side.
pub(crate) struct PortCreditReturner {
    monitor: Weak<PortCreditMonitorInner>,
    to_return: u32,
    tasks: VecDeque<BoxFuture<'static, ()>>,
}

impl PortCreditReturner {
    /// Return quotient.
    const RETURN_QUOT: u32 = 4;

    /// Ensures the credit returner is ready.
    ///
    /// Must be called before calling [Self::start_return].
    pub async fn ready(&mut self) {
        while let Some(task) = self.tasks.front_mut() {
            task.await;
            self.tasks.pop_front();
        }
    }

    fn queue_port_evt(&mut self, tx: &mpsc::Sender<PortEvt>, evt: PortEvt) {
        if let Err(TrySendError::Full(evt)) = tx.try_send(evt) {
            let tx = tx.clone();
            let task = async move {
                let _ = tx.send(evt).await;
            };
            self.tasks.push_back(task.boxed());
        }
    }

    /// Starts returning port-specific credit.
    ///
    /// [Self::ready] must have been called before this function is called.
    pub fn start_return(&mut self, credit: UsedPortCredit, remote_port: u32, tx: &mpsc::Sender<PortEvt>) {
        assert!(self.tasks.is_empty(), "start_return called without ready");

        let Some(monitor) = self.monitor.upgrade() else { return };

        // Make sure remote endpoint has at least 4 credits (size of u32),
        // to be able to send a port data message with one port chunk.
        let mut return_threshold = (monitor.port_limit / Self::RETURN_QUOT).max(1);
        if monitor.port_limit - return_threshold <= size_of::<u32>() as u32 {
            return_threshold = 1;
        }

        let port_used = monitor.port_used.fetch_sub(credit.port, Ordering::Relaxed) - credit.port;
        let global_used = monitor.global_used.fetch_sub(credit.global, Ordering::Relaxed) - credit.global;

        self.to_return += credit.port;
        if self.to_return >= return_threshold {
            self.queue_port_evt(tx, PortEvt::ReturnCredits { remote_port, credits: self.to_return });
            self.to_return = 0;
        }

        let total_used = port_used.saturating_add(global_used);
        if total_used < monitor.throttle {
            match monitor
                .global_credit_usage
                .swap(PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_ACTIVE, Ordering::Relaxed)
            {
                PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_ACTIVE => (),
                PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_INHIBITING => (),
                PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_INHIBITED => {
                    self.queue_port_evt(tx, PortEvt::ChangeGlobalCreditUsage { remote_port, allow: true });
                }
                _ => unreachable!("invalid global_credit_usage state"),
            }
        }
    }

    /// Starts reporting that messages have been processed.
    ///
    /// [Self::ready] must have been called before this function is called.
    pub fn start_report_processed(&mut self, remote_port: u32, tx: &mpsc::Sender<PortEvt>) {
        assert!(self.tasks.is_empty(), "report_received called without ready");
        self.queue_port_evt(tx, PortEvt::ReceivedReport { remote_port });
    }
}

/// A pair of [PortCreditMonitor] and [PortCreditReturner].
pub(crate) fn port_credit_monitor(limit: u32, throttle: u32) -> (PortCreditMonitor, PortCreditReturner) {
    let inner = Arc::new(PortCreditMonitorInner {
        port_used: 0.into(),
        port_limit: limit,
        throttle,
        global_used: 0.into(),
        global_credit_usage: PortCreditMonitorInner::GLOBAL_CREDIT_USAGE_ACTIVE.into(),
    });
    let returner = PortCreditReturner { monitor: Arc::downgrade(&inner), to_return: 0, tasks: VecDeque::new() };
    let monitor = PortCreditMonitor(inner);
    (monitor, returner)
}

/// Represents monitored used global credits.
#[derive(Default, Debug)]
pub(crate) struct UsedGlobalCredit {
    credits: u32,
    monitor: Weak<GlobalCreditMonitor>,
}

impl fmt::Display for UsedGlobalCredit {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.credits)
    }
}

impl UsedGlobalCredit {
    /// Credits in bytes.
    pub fn credits(&self) -> u32 {
        self.credits
    }
}

impl Drop for UsedGlobalCredit {
    fn drop(&mut self) {
        if let Some(monitor) = self.monitor.upgrade() {
            monitor.return_credits(self);
        }
    }
}

/// Monitors global credit usage.
pub(crate) struct GlobalCreditMonitor {
    /// Total number of credits assigned buffer sizer.
    total: AtomicU32,
    /// Current number of credits in-use by received data.
    used: AtomicU32,
    /// Current number of credits not yet returned to remote endpoint.
    to_return: AtomicU32,
    /// Threshold of [Self::to_return] that triggers returning credits to remote endpoint.
    return_threshold: AtomicU32,
    /// Notification for when credits can be returned to remote endpoint.
    return_notify: Notify,
    /// Sequence number of global credits report.
    seq: AtomicU8,
}

impl GlobalCreditMonitor {
    /// Creates a new credit monitor.
    pub fn new(initial: BufferSize) -> Arc<Self> {
        Arc::new(Self {
            total: initial.size.into(),
            used: 0.into(),
            to_return: 0.into(),
            return_threshold: initial.return_threshold.into(),
            seq: 0.into(),
            return_notify: Notify::new(),
        })
    }

    /// Total number of credits.
    pub fn total(&self) -> u32 {
        self.total.load(Ordering::Relaxed)
    }

    /// Threshold that triggers returning credits to remote endpoint.
    pub fn return_threshold(&self) -> u32 {
        self.return_threshold.load(Ordering::Relaxed)
    }

    /// Use global credits.
    pub fn use_credits<SinkError, StreamError>(
        self: &Arc<Self>, credits: u32,
    ) -> Result<UsedGlobalCredit, ChMuxError<SinkError, StreamError>> {
        match self.used.fetch_add(credits, Ordering::Relaxed).checked_add(credits) {
            Some(new_used) if new_used <= self.total.load(Ordering::Relaxed) => {
                Ok(UsedGlobalCredit { credits, monitor: Arc::downgrade(self) })
            }
            _ => Err(ChMuxError::Protocol("remote endpoint used too many global flow credits".to_string())),
        }
    }

    /// Return global credits.
    fn return_credits(&self, credit: &mut UsedGlobalCredit) {
        let credits = mem::take(&mut credit.credits);
        if credits == 0 {
            return;
        }

        self.used.fetch_sub(credits, Ordering::Relaxed);

        let Some(to_return) = self.to_return.fetch_add(credits, Ordering::Relaxed).checked_add(credits) else {
            panic!("global return credit overflow")
        };

        if to_return > 0 && to_return >= self.return_threshold() {
            self.return_notify.notify_one();
        }
    }

    /// Wait for returnable credits.
    pub async fn wait_for_returnable(&self) {
        self.return_notify.notified().await
    }

    /// Returns how many credits should be sent to the remote endpoint.
    pub fn return_to_remote(
        &self, buffer_sizer: &mut dyn BufferSizer, report: &GlobalCreditsReport,
    ) -> Option<GlobalCredits> {
        let mut seq = self.seq.load(Ordering::Relaxed);
        let mut total = self.total();

        // Query buffer sizer.
        let query = BufferSizeQuery {
            current_size: total,
            used: self.used.load(Ordering::Relaxed),
            returnable: self.to_return.load(Ordering::Relaxed),
            seq,
            report,
            report_is_current: report.seq == seq,
        };
        let target = buffer_sizer.size(query);

        // Adjust total credits in circulation, if requested by buffer manager.
        let total_changed = if target.size > total {
            // If buffer manager added credits, add them to the return pool.
            let additional = target.size - total;
            self.to_return.fetch_add(additional, Ordering::Relaxed);
            total = target.size;
            tracing::trace!(%total, "added {additional} global flow credits");
            true
        } else if target.size < total {
            // If buffer manager removed credits, consume them from the return pool.
            let superflous = total - target.size;
            let mut removed = 0;
            self.to_return.update(Ordering::Relaxed, Ordering::Relaxed, |to_return| {
                removed = superflous.min(to_return);
                to_return - removed
            });
            if removed > 0 {
                total -= removed;
                tracing::trace!(%total, "removed {removed} global flow credits");
                true
            } else {
                false
            }
        } else {
            false
        };

        // Increment sequence number, if total credit count changed.
        if total_changed {
            seq = seq.wrapping_add(1);
            self.seq.store(seq, Ordering::Relaxed);
            self.total.store(total, Ordering::Relaxed);
        }

        // Update return threshold.
        assert!(0 < target.return_threshold && target.return_threshold <= target.size);
        self.return_threshold.store(target.return_threshold, Ordering::Relaxed);

        // Send credits to remote endpoint if required.
        let returning = self
            .to_return
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |to_return| {
                if total_changed || target.force_return || to_return >= target.return_threshold {
                    Some(0)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        if total_changed || returning > 0 { Some(GlobalCredits { credits: returning, seq }) } else { None }
    }
}

/// Requests and consumes credits for sending from both global and port credits.
pub struct MixedCreditUser {
    /// Port credit user.
    pub port: CreditUser,
    /// Global credit user.
    pub global: Arc<Option<CreditUser>>,
    /// Whether global credit use is enabled.
    pub global_enabled: bool,
}

impl MixedCreditUser {
    /// Create a new instance.
    pub fn new(port: CreditUser, global: Arc<Option<CreditUser>>) -> Self {
        Self { port, global, global_enabled: true }
    }

    /// Requests the specified number of credits for sending without blocking.
    /// Returns credits up to `req` if at least `min_req` available, otherwise None.
    ///
    /// Prefers global credits and fills up with port credits.
    pub fn try_request(&self, mut req: u32, mut min_req: u32) -> Result<Option<MixedAssignedCredits>, SendError> {
        debug_assert!(req >= min_req);

        self.port.check_sendable()?;

        let mut mixed = MixedAssignedCredits::new();

        'acquire: {
            if let Some(credits) = self.port.try_request(req, min_req)? {
                req = req.saturating_sub(credits.available());
                min_req = min_req.saturating_sub(credits.available());
                mixed.add(credits);
            }

            if req == 0 {
                break 'acquire;
            }

            if self.global_enabled
                && self.port.remote_allows_global_credits()
                && let Some(global) = &*self.global
                && let Some(credits) = global.try_request(req, min_req)?
            {
                min_req = min_req.saturating_sub(credits.available());
                mixed.add(credits);
            }
        }

        if min_req == 0 { Ok(Some(mixed)) } else { Ok(None) }
    }

    /// Requests credits for sending.
    /// Blocks until at least `min_req` credits become available.
    ///
    /// Prefers global credits and fills up with port credits.
    pub async fn request(&self, mut req: u32, mut min_req: u32) -> Result<MixedAssignedCredits, SendError> {
        debug_assert!(req >= min_req);

        self.port.check_sendable()?;

        let mut mixed = MixedAssignedCredits::new();

        'acquire: {
            match &*self.global {
                Some(global) if self.global_enabled => {
                    if let Some(credits) = self.port.try_request(req, min_req)? {
                        req = req.saturating_sub(credits.available());
                        min_req = min_req.saturating_sub(credits.available());
                        mixed.add(credits);
                    }

                    if req == 0 {
                        break 'acquire;
                    }

                    if self.port.remote_allows_global_credits()
                        && let Some(credits) = global.try_request(req, min_req)?
                    {
                        req = req.saturating_sub(credits.available());
                        min_req = min_req.saturating_sub(credits.available());
                        mixed.add(credits);
                    }

                    if min_req == 0 {
                        break 'acquire;
                    }

                    let global_req = async {
                        self.port.wait_for_remote_allowing_global_credits().await;
                        global.request(req, min_req).await
                    };

                    tokio::select! {
                        biased;
                        credits = self.port.request(req, min_req) => mixed.add(credits?),
                        credits = global_req => mixed.add(credits?),
                    }
                }
                _ => {
                    let credits = self.port.request(req, min_req).await?;
                    mixed.add(credits);
                }
            }
        }

        Ok(mixed)
    }
}
