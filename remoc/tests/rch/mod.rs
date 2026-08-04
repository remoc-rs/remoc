mod bin;
mod broadcast;
mod io;
mod lr;
mod mpsc;
mod oneshot;
mod remote;
mod watch;

/// Assert fmt::Debug impls for error types containing non-Debug data.
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

/// Assert Send and Sync for channel types.
const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<remoc::rch::base::Sender<u32>>();
    assert_send::<remoc::rch::base::Receiver<u32>>();
    assert_send_sync::<remoc::rch::broadcast::Sender<u32>>();
    assert_send_sync::<remoc::rch::broadcast::Receiver<u32>>();
    assert_send_sync::<remoc::rch::lr::Sender<u32>>();
    assert_send::<remoc::rch::lr::Receiver<u32>>();
    assert_send_sync::<remoc::rch::mpsc::Sender<u32>>();
    assert_send_sync::<remoc::rch::mpsc::Receiver<u32>>();
    assert_send_sync::<remoc::rch::oneshot::Sender<u32>>();
    assert_send_sync::<remoc::rch::oneshot::Receiver<u32>>();
    assert_send_sync::<remoc::rch::watch::Sender<u32>>();
    assert_send_sync::<remoc::rch::watch::Receiver<u32>>();

    assert_send_sync::<remoc::codec::ErasedSerializer>();
    assert_send_sync::<remoc::codec::ErasedDeserializer>();
    assert_send_sync::<remoc::rch::base::ErasedSender>();
    assert_send::<remoc::rch::base::ErasedReceiver>();
};
