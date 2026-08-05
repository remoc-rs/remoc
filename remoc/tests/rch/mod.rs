mod bin;
mod broadcast;
mod io;
mod lr;
mod mpsc;
mod oneshot;
mod remote;
mod watch;

/// Send errors must implement [`Debug`](std::fmt::Debug) even if the item type does not,
/// since [`RemoteSend`](remoc::RemoteSend) does not require it.
const _: () = {
    struct NotDebug;

    const fn assert_debug<T: std::fmt::Debug>() {}

    assert_debug::<remoc::rch::SendingError<NotDebug>>();
    assert_debug::<remoc::rch::base::SendError<NotDebug>>();
    assert_debug::<remoc::rch::broadcast::SendError<NotDebug>>();
    assert_debug::<remoc::rch::lr::SendError<NotDebug>>();
    assert_debug::<remoc::rch::mpsc::SendError<NotDebug>>();
    assert_debug::<remoc::rch::mpsc::TrySendError<NotDebug>>();
    assert_debug::<remoc::rch::oneshot::SendError<NotDebug>>();
};
