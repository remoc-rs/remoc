use remoc::prelude::*;
use std::{fs::File, io::BufReader, path::Path, sync::Arc};
use tokio::net::TcpListener;
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer},
    },
};

/// Serves Remoc endpoints over TLS-secured TCP connections.
pub async fn serve(addr: &str, cert_pem: &Path, key_pem: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(File::open(cert_pem)?)).collect::<Result<_, _>>()?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut BufReader::new(File::open(key_pem)?))?
        .ok_or("no private key in key file")?;

    let config = ServerConfig::builder().with_no_client_auth().with_single_cert(certs, key)?;
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind(addr).await?;

    loop {
        let (tcp, _peer) = listener.accept().await?;
        let acceptor = acceptor.clone();

        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else { return };

            // Any AsyncRead and AsyncWrite pair works, so the TLS stream is simply split.
            let (tls_rx, tls_tx) = tokio::io::split(tls);

            let Ok((conn, tx, rx)) = remoc::Connect::io(remoc::Cfg::default(), tls_rx, tls_tx).await else {
                return;
            };
            tokio::spawn(conn);

            serve_client(tx, rx).await;
        });
    }
}

async fn serve_client(mut tx: rch::base::Sender<String>, mut rx: rch::base::Receiver<String>) {
    while let Ok(Some(msg)) = rx.recv().await {
        if tx.send(msg.to_uppercase()).await.is_err() {
            break;
        }
    }
}
