use crate::{MyInitialReq, MyInitialRsp};
use quinn::rustls::RootCertStore;
use remoc::prelude::*;
use std::{net::SocketAddr, sync::Arc};

/// Connects to a Remoc endpoint over QUIC.
///
/// `server_name` must match the certificate of the server.
pub async fn connect(
    addr: SocketAddr, server_name: &str,
) -> Result<
    (rch::base::Sender<MyInitialReq>, rch::base::Receiver<MyInitialRsp>),
    Box<dyn std::error::Error>,
> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = quinn::ClientConfig::with_root_certificates(Arc::new(roots))?;

    let mut endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;
    endpoint.set_default_client_config(config);

    let connection = endpoint.connect(addr, server_name)?.await?;
    let (quic_tx, quic_rx) = connection.open_bi().await?;

    let (conn, tx, rx) =
        remoc::Connect::io(remoc::Cfg::default(), quic_rx, quic_tx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}
