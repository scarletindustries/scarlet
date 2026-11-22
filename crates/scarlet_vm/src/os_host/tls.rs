//! TLS client sessions, on rustls with aws-lc's crypto.
//!
//! The peer's certificate is always checked, and there is no way to skip it.
//! Trust comes from the machine's own store first, so a CA an administrator
//! added is honoured, with the bundled Mozilla set when the machine has none.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::host::{Received, TlsError};

use super::net_error;

/// The client settings every handshake in a run shares. Sessions resume from
/// an in-memory cache hung off it, never one on disk.
#[derive(Clone)]
pub(super) struct Config(Arc<ClientConfig>);

impl Config {
    pub(super) fn new() -> Result<Config, TlsError> {
        let mut roots = RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            // A store with one bad certificate in it is still a store.
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| TlsError::HandshakeFailed)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Config(Arc::new(config)))
    }
}

pub(super) struct Session(StreamOwned<ClientConnection, TcpStream>);

impl Session {
    /// Connect over `tcp` as a client of `server_name`. A name that is an IP
    /// address is checked against the certificate's IP addresses. A timeout
    /// drops the connection, so there is nothing left to retry on.
    pub(super) fn handshake(
        config: &Config,
        mut tcp: TcpStream,
        server_name: &str,
        wait: Option<Duration>,
    ) -> Result<Session, TlsError> {
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|_| TlsError::InvalidServerName)?;
        let mut conn =
            ClientConnection::new(config.0.clone(), name).map_err(|e| session_error(&e))?;
        set_timeouts(&tcp, wait)?;
        while conn.is_handshaking() {
            conn.complete_io(&mut tcp).map_err(|e| tls_error(&e))?;
        }
        set_timeouts(&tcp, None)?;
        Ok(Session(StreamOwned::new(conn, tcp)))
    }

    // A peer that closes without saying so first looks the same as one that
    // was cut off. Both are `Closed`; `scarlet/net/tls` says so.
    pub(super) fn read(
        &mut self,
        max: usize,
        wait: Option<Duration>,
    ) -> Result<Received, TlsError> {
        self.0
            .sock
            .set_read_timeout(wait)
            .map_err(|e| TlsError::Transport(net_error(&e)))?;
        let mut buf = vec![0u8; max];
        loop {
            match self.0.read(&mut buf) {
                Ok(0) => return Ok(Received::Closed),
                Ok(n) => {
                    buf.truncate(n);
                    return Ok(Received::Data(buf));
                }
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(Received::Closed),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(tls_error(&e)),
            }
        }
    }

    pub(super) fn write(&mut self, data: &[u8]) -> Result<(), TlsError> {
        self.0.write_all(data).map_err(|e| tls_error(&e))?;
        self.0.flush().map_err(|e| tls_error(&e))
    }

    pub(super) fn close(&mut self) -> Result<(), TlsError> {
        self.0.conn.send_close_notify();
        let sent = self.0.flush().map_err(|e| tls_error(&e));
        let _ = self.0.sock.shutdown(std::net::Shutdown::Both);
        sent
    }
}

fn set_timeouts(tcp: &TcpStream, wait: Option<Duration>) -> Result<(), TlsError> {
    tcp.set_read_timeout(wait)
        .and_then(|()| tcp.set_write_timeout(wait))
        .map_err(|e| TlsError::Transport(net_error(&e)))
}

/// What an I/O error from a session stands for: the session's own failure
/// when rustls made it, or the network's.
fn tls_error(e: &std::io::Error) -> TlsError {
    match e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(r) => session_error(r),
        None => TlsError::Transport(net_error(e)),
    }
}

/// Certificate failures are told apart by cause, because a caller acts on
/// each differently: an unknown issuer usually means a root missing on this
/// machine, and a name mismatch means the caller asked for the wrong host.
fn session_error(e: &rustls::Error) -> TlsError {
    use rustls::{CertificateError, Error};
    match e {
        Error::InvalidCertificate(c) => match c {
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                TlsError::CertificateExpired
            }
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                TlsError::CertificateNotYetValid
            }
            CertificateError::Revoked => TlsError::CertificateRevoked,
            CertificateError::UnknownIssuer => TlsError::CertificateUnknownIssuer,
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                TlsError::HostnameMismatch
            }
            _ => TlsError::BadCertificate,
        },
        // A peer that answers a ClientHello with something that isn't TLS,
        // like an `http` port, lands here.
        Error::InvalidMessage(_)
        | Error::NoCertificatesPresented
        | Error::UnsupportedNameType
        | Error::DecryptError
        | Error::EncryptError
        | Error::PeerIncompatible(_) => TlsError::ProtocolError,
        _ => TlsError::HandshakeFailed,
    }
}
