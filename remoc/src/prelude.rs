//! Convenience re-export of common members.
//!
//! Like the standard library's prelude, this module simplifies importing of common items.
//! Unlike the standard prelude, the contents of this module must be imported manually.
//!
//! ```
//! use remoc::prelude::*;
//! ```
//!

pub use crate::chmux;

#[cfg(feature = "rch")]
pub use crate::rch;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::ConnectExt;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::RemoteSend;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::rch::SendResultExt;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::rch::base::BaseExt;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::rch::mpsc::MpscExt;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::rch::oneshot::OneshotExt;

#[cfg(feature = "rch")]
#[doc(no_inline)]
pub use crate::rch::watch::WatchExt;

#[cfg(feature = "rfn")]
pub use crate::rfn;

#[cfg(feature = "robj")]
pub use crate::robj;

#[cfg(feature = "robs")]
pub use crate::robs;

#[cfg(feature = "rtc")]
pub use crate::rtc;

#[cfg(feature = "rtc")]
#[doc(no_inline)]
pub use crate::rtc::{
    Client, MonitorableClient, MonitorableReqReceiver, MonitorableServer, ReqReceiver, Server, ServerRef,
    ServerRefMut, ServerShared, ServerSharedMut,
};
