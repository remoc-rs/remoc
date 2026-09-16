use crate::{MyInitialReq, MyInitialRsp};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use remoc::prelude::*;
use std::{fs::File, io::BufReader, net::SocketAddr, path::Path};

/// Serves Remoc endpoints over QUIC.
pub async fn serve(
    addr: SocketAddr, cert_pem: &Path, key_pem: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(File::open(cert_pem)?))
            .collect::<Result<_, _>>()?;
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut BufReader::new(File::open(key_pem)?))?
            .ok_or("no private key in key file")?;

    let config = quinn::ServerConfig::with_single_cert(certs, key)?;
    let endpoint = quinn::Endpoint::server(config, addr)?;

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else { return };
            let Ok((quic_tx, quic_rx)) = connection.accept_bi().await else { return };

            let Ok((conn, tx, rx)) =
                remoc::Connect::io(remoc::Cfg::default(), quic_rx, quic_tx).await
            else {
                return;
            };
            tokio::spawn(conn);

            serve_client(tx, rx).await;
        });
    }

    Ok(())
}

async fn serve_client(
    mut tx: rch::base::Sender<MyInitialRsp>, mut rx: rch::base::Receiver<MyInitialReq>,
) {
    while let Ok(Some(_req)) = rx.recv().await {
        // Handle the initial request here; from this point on your application
        // exchanges further channels and remote objects over the connection.
        if tx.send(MyInitialRsp {}).await.is_err() {
            break;
        }
    }
}
