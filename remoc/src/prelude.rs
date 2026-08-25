//! Convenient imports for typical Remoc applications.
//!
//! Unlike Rust's standard prelude, this module must be imported explicitly:
//!
//! ```
//! use remoc::prelude::*;
//! ```
//!
//! This brings the enabled high-level modules into scope, together with extension
//! traits used for connections and channels. It intentionally does not re-export
//! every concrete channel or remote-object type; those remain namespaced, for
//! example as `rch::mpsc::Sender` and `rtc::CallError`.
//!
//! Importing the prelude is optional. Libraries may prefer explicit imports in
//! their public APIs, while applications and examples often benefit from the
//! shorter names.
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
pub use crate::rtc::monitor::{MonitorableClient, MonitorableReqReceiver, MonitorableServer};

#[cfg(feature = "rtc")]
#[doc(no_inline)]
pub use crate::rtc::{
    CallFutureExt, Client, ReqReceiver, Server, ServerBase, ServerRef, ServerRefMut, ServerShared,
    ServerSharedMut,
};
