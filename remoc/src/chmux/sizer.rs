//! Dynamic receive buffer sizing.

use std::fmt;

/// Report of available credits for sending data.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GlobalCreditsReport {
    /// Currently held send credits in bytes.
    pub current: u32,
    /// Minimum held send credits in bytes since last credits report.
    pub min: u32,
    /// Last received sequence number.
    pub(crate) seq: u8,
}

impl GlobalCreditsReport {
    /// Creates initial report.
    pub(crate) fn initial(credits: u32) -> Self {
        Self { current: credits, min: credits, seq: 0 }
    }

    /// Consume credits.
    pub(crate) fn consume(&mut self, credits: u32) {
        self.current = self.current.saturating_sub(credits);
        self.min = self.min.min(self.current);
    }
}

/// Buffer size query.
#[derive(Debug)]
#[non_exhaustive]
pub struct BufferSizeQuery<'a> {
    /// Current buffer size in bytes.
    pub current_size: u32,
    /// Used buffer size in bytes.
    pub used: u32,
    /// Credits currently returnable to remote endpoint in bytes.
    pub returnable: u32,
    /// Current sequence number.
    ///
    /// Incremented each time [BufferSize::size] changes.
    pub seq: u8,
    /// Credits usage report of remote endpoint.
    pub report: &'a GlobalCreditsReport,
    /// Whether credit usage report was created after last buffer size adjustment.
    pub report_is_current: bool,
}

/// Buffer size.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BufferSize {
    /// New receive buffer size in bytes.
    pub size: u32,
    /// Threshold for returning credits to remote endpoint.
    pub return_threshold: u32,
    /// Whether to return credits now, even if currently under [Self::return_threshold].
    pub force_return: bool,
}

impl BufferSize {
    /// Creates a new buffer size with default return threshold.
    pub fn new(size: u32) -> Self {
        Self { size, return_threshold: (size / 10).clamp(1, 65_536), force_return: false }
    }
}

/// Determines the target receive buffer size.
pub trait BufferSizer: fmt::Debug + Send + Sync + 'static {
    /// Create a new instance for use with a separate channel multiplexer.
    ///
    /// Configuration should be duplicated, while state should be reset.
    fn duplicate(&self) -> Box<dyn BufferSizer>;

    /// Initial buffer configuration.
    fn initial(&mut self) -> BufferSize;

    /// Compute target size for receive buffer.
    ///
    /// The passed global credits report shows how many send credits the
    /// remote endpoint has. `current` is true when `report` has been received
    /// after the last receive buffer size change.
    fn size<'a>(&mut self, query: BufferSizeQuery<'a>) -> BufferSize;
}

impl Clone for Box<dyn BufferSizer> {
    fn clone(&self) -> Self {
        self.duplicate()
    }
}

/// Dummy sizer for moving the real sizer out of the configuration struct.
#[derive(Debug)]
pub(crate) struct DummySizer;

impl DummySizer {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Box<dyn BufferSizer> {
        Box::new(Self)
    }
}

impl BufferSizer for DummySizer {
    fn duplicate(&self) -> Box<dyn BufferSizer> {
        unreachable!()
    }

    fn initial(&mut self) -> BufferSize {
        unreachable!()
    }

    fn size<'a>(&mut self, state: BufferSizeQuery<'a>) -> BufferSize {
        let _ = state;
        unreachable!()
    }
}

/// Buffer sizer that has constant size.
#[derive(Debug, Clone)]
pub struct FixedBuffer(BufferSize);

impl FixedBuffer {
    /// Creates a buffer sizer that has the specified constant size in bytes.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(size: u32) -> Box<dyn BufferSizer> {
        Box::new(Self(BufferSize::new(size)))
    }

    /// Receive buffer size in bytes.
    pub const fn size(&self) -> u32 {
        self.0.size
    }
}

impl BufferSizer for FixedBuffer {
    fn duplicate(&self) -> Box<dyn BufferSizer> {
        Box::new(self.clone())
    }

    fn initial(&mut self) -> BufferSize {
        self.0.clone()
    }

    fn size<'a>(&mut self, query: BufferSizeQuery<'a>) -> BufferSize {
        let _ = query;
        self.0.clone()
    }
}

/// Buffer sizer that has constant size.
#[derive(Debug, Clone)]
pub struct DynamicBuffer {
    min: u32,
    max: u32,
    /// Level quotient.
    pub level_quot: u32,
    current: BufferSize,
    low_level: u32,
    high_level: u32,
    record_max: u32,
}

impl DynamicBuffer {
    /// Create a dynamic buffer sizer with specified limits.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(min: u32, max: u32) -> Box<dyn BufferSizer> {
        assert!(min <= max);
        let this = Self {
            min,
            max,
            level_quot: 2,
            current: BufferSize::new(0),
            low_level: 0,
            high_level: 0,
            record_max: 0,
        };
        Box::new(this)
    }

    fn set_size(&mut self, mut size: u32) {
        size = size.clamp(self.min, self.max);
        if self.current.size == size {
            return;
        }

        self.current = BufferSize::new(size);
        self.low_level = (size / self.level_quot).clamp(1024, 1_048_576);
        self.high_level = 4 * self.low_level;

        const MB: f32 = 1_048_576.;
        tracing::debug!("adjusting receive buffer size to {:.1} MB", size as f32 / MB);
        if size > self.record_max {
            self.record_max = size;
            tracing::info!("maximum receive buffer size is {:.1} MB", self.record_max as f32 / MB);
        }
    }
}

impl BufferSizer for DynamicBuffer {
    fn duplicate(&self) -> Box<dyn BufferSizer> {
        Box::new(self.clone())
    }

    fn initial(&mut self) -> BufferSize {
        self.set_size(self.min);
        self.current.clone()
    }

    fn size<'a>(&mut self, query: BufferSizeQuery<'a>) -> BufferSize {
        // Only adjust credits if report is current.
        if !query.report_is_current || query.current_size != self.current.size {
            return self.current.clone();
        }

        tracing::trace!("computing receive buffer size: {query:?}");

        if query.report.min < self.low_level {
            self.set_size(self.current.size.saturating_mul(4));
        } else if query.report.min > self.high_level {
            let diff = ((query.report.min - self.high_level) / 2).min(65_536);
            self.set_size(self.current.size.saturating_sub(diff));
        }

        self.current.clone()
    }
}
