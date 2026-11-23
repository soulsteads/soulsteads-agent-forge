use crate::settings::TlsConfig;
use anyhow::{Context, Result};
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        crypto::aws_lc_rs,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
    server::TlsStream,
};

pub(crate) fn acceptor(config: &TlsConfig) -> Result<TlsAcceptor> {
    let certificates = CertificateDer::pem_file_iter(&config.certificate)
        .context("cannot read server.tls certificate")?
        .collect::<Result<Vec<_>, _>>()
        .context("invalid server.tls certificate PEM")?;
    let key = crate::safety::read_private(&config.private_key)
        .context("server.tls private_key must be a readable owner-only regular file")?;
    let key = PrivateKeyDer::from_pem_slice(&key).context("invalid server.tls private_key PEM")?;
    let tls = ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .context("invalid server.tls certificate and private_key pair")?;
    Ok(TlsAcceptor::from(Arc::new(tls)))
}

pub(crate) struct Listener {
    tcp: TcpListener,
    tls: TlsAcceptor,
    pending: JoinSet<Option<(TlsStream<TcpStream>, SocketAddr)>>,
}

impl Listener {
    pub(crate) fn new(tcp: TcpListener, tls: TlsAcceptor) -> Self {
        Self {
            tcp,
            tls,
            pending: JoinSet::new(),
        }
    }
}

impl axum::serve::Listener for Listener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                result = self.pending.join_next(), if !self.pending.is_empty() => {
                    if let Some(Ok(Some(accepted))) = result {
                        return accepted;
                    }
                }
                (stream, addr) = axum::serve::Listener::accept(&mut self.tcp), if self.pending.len() < 64 => {
                    let acceptor = self.tls.clone();
                    self.pending.spawn(async move {
                        let stream = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream))
                            .await.ok()?.ok()?;
                        Some((stream, addr))
                    });
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}
