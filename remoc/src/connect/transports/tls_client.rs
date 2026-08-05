use remoc::prelude::*;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};

/// Connects to a Remoc endpoint over a TLS-secured TCP connection.
pub async fn connect(
    host: &str, port: u16,
) -> Result<(rch::base::Sender<String>, rch::base::Receiver<String>), Box<dyn std::error::Error>> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let tcp = TcpStream::connect((host, port)).await?;
    let tls = connector.connect(ServerName::try_from(host)?.to_owned(), tcp).await?;

    // Any AsyncRead and AsyncWrite pair works, so the TLS stream is simply split.
    let (tls_rx, tls_tx) = tokio::io::split(tls);

    let (conn, tx, rx) = remoc::Connect::io(remoc::Cfg::default(), tls_rx, tls_tx).await?;
    tokio::spawn(conn);

    Ok((tx, rx))
}
