//! FTPS transport — explicit TLS over the FTP control channel via rustls.
//!
//! ## Why rustls instead of native-tls
//!
//! suppaftp 8's tokio + native-tls path has a Windows compile bug
//! (uses `std::os::fd::AsFd` without a Unix-only cfg gate). rustls is
//! pure-Rust, so it cross-compiles cleanly to every platform we care about
//! and has no system OpenSSL dependency on Linux. Trust anchors come from
//! `webpki-roots`, which embeds the Mozilla CA bundle at compile time.
//!
//! ## Wire-level behavior
//!
//! Connects via plain `AsyncRustlsFtpStream::connect`, then upgrades via
//! `into_secure(connector, hostname)` — explicit FTPS, RFC 4217. Implicit
//! FTPS on the deprecated port 990 isn't supported.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use suppaftp::list::File as FtpFile;
use suppaftp::rustls::{ClientConfig, RootCertStore};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{self, BlinkError, Result};
use crate::session::{AuthMethod, Protocol, Session};
use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry, Transport};

/// Cap on bytes read by `read_to_bytes` — matches the image preview limit.
/// Prevents a server that lies about file size in its listing from causing
/// unbounded allocation via `read_to_end`.
const MAX_PREVIEW_BYTES: u64 = 10_000_000; // 10 MB

pub struct FtpsTransport {
    /// The post-secure stream type. After `into_secure` the same value
    /// continues to be referenced as `AsyncRustlsFtpStream`; the type alias
    /// in suppaftp wraps the parameterized form.
    stream: AsyncRustlsFtpStream,
}

impl FtpsTransport {
    pub async fn connect(session: &Session, password: Option<&str>) -> Result<Self> {
        if !matches!(session.auth, AuthMethod::Password) {
            return Err(BlinkError::auth(
                "FTPS only supports password (or anonymous) auth",
            ));
        }

        let addr = format!("{}:{}", session.host, session.port);
        let plain = AsyncRustlsFtpStream::connect(&addr).await.map_err(|e| {
            BlinkError::connect(format!("ftps connect to {addr}: {e}"))
        })?;

        // rustls config. The default path uses webpki-roots for trust anchors;
        // the `accept_invalid_certs` opt-in installs a custom verifier that
        // accepts every server certificate. rustls deliberately makes the
        // dangerous path verbose — we name it `NoVerification` to keep the
        // intent obvious in stack traces.
        let config = if session.accept_invalid_certs {
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(
                    no_verification::NoVerification,
                ))
                .with_no_client_auth()
        } else {
            let root_store = RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            );
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        let connector = AsyncRustlsConnector::from(
            tokio_rustls::TlsConnector::from(Arc::new(config)),
        );
        let mut stream = plain
            .into_secure(connector, &session.host)
            .await
            .map_err(|e| BlinkError::connect(format!("ftps tls upgrade: {e}")))?;

        let (user, pw) = if session.username.is_empty() {
            ("anonymous", "anonymous@")
        } else {
            let pw = password.unwrap_or("");
            (session.username.as_str(), pw)
        };
        stream
            .login(user, pw)
            .await
            .map_err(|e| BlinkError::auth(format!("ftps login: {e}")))?;

        stream
            .transfer_type(FileType::Binary)
            .await
            .map_err(|e| BlinkError::transport(format!("set binary: {e}")))?;

        Ok(Self { stream })
    }
}

#[async_trait]
impl Transport for FtpsTransport {
    fn protocol(&self) -> Protocol {
        Protocol::Ftps
    }

    async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>> {
        let lines = self
            .stream
            .list(Some(remote_path))
            .await
            .map_err(|e| BlinkError::transport(format!("list {remote_path}: {e}")))?;

        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            if line.starts_with("total ") {
                continue;
            }
            let parsed = match FtpFile::from_str(&line) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let name = error::sanitize(parsed.name().to_string());
            if name == "." || name == ".." {
                continue;
            }
            let kind = if parsed.is_directory() {
                EntryKind::Directory
            } else if parsed.is_symlink() {
                EntryKind::Symlink
            } else if parsed.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            out.push(RemoteEntry {
                name,
                kind,
                size: parsed.size() as u64,
                modified: None,
                mode: None,
            });
        }
        Ok(out)
    }

    async fn download(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()> {
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let local_path = local_path.to_path_buf();
        let progress_clone = progress.clone();
        let total = self.stream.size(remote_path).await.unwrap_or(0) as u64;
        let remote_path_owned = remote_path.to_string();

        self.stream
            .retr(&remote_path_owned, move |mut reader| {
                let local_path = local_path.clone();
                let progress = progress_clone.clone();
                Box::pin(async move {
                    let mut file = tokio::fs::File::create(&local_path)
                        .await
                        .map_err(|e| {
                            suppaftp::FtpError::ConnectionError(std::io::Error::other(
                                format!("create {}: {e}", local_path.display()),
                            ))
                        })?;
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut done: u64 = 0;
                    loop {
                        let n = reader.read(&mut buf).await.map_err(
                            suppaftp::FtpError::ConnectionError,
                        )?;
                        if n == 0 {
                            break;
                        }
                        file.write_all(&buf[..n]).await.map_err(
                            suppaftp::FtpError::ConnectionError,
                        )?;
                        done += n as u64;
                        if let Some(tx) = &progress {
                            let _ = tx.send(ProgressUpdate {
                                bytes_done: done,
                                bytes_total: total,
                            });
                        }
                    }
                    file.flush()
                        .await
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    Ok(((), reader))
                })
            })
            .await
            .map_err(|e| BlinkError::transport(format!("retr {remote_path}: {e}")))?;
        Ok(())
    }

    async fn upload(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()> {
        let total = tokio::fs::metadata(local_path).await?.len();
        let mut local = tokio::fs::File::open(local_path).await?;

        let mut writer = self
            .stream
            .put_with_stream(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("stor {remote_path}: {e}")))?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut done: u64 = 0;
        loop {
            let n = local.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .await
                .map_err(|e| BlinkError::transport(format!("write: {e}")))?;
            done += n as u64;
            if let Some(tx) = &progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: done,
                    bytes_total: total,
                });
            }
        }
        writer
            .flush()
            .await
            .map_err(|e| BlinkError::transport(format!("flush: {e}")))?;
        self.stream
            .finalize_put_stream(writer)
            .await
            .map_err(|e| BlinkError::transport(format!("finalize put: {e}")))?;
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.stream
            .rename(from, to)
            .await
            .map_err(|e| BlinkError::transport(format!("rename {from} -> {to}: {e}")))
    }

    async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
        self.stream
            .rm(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("dele {remote_path}: {e}")))
    }

    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
        if !recursive {
            return self.stream.rmdir(remote_path).await.map_err(|e| {
                BlinkError::transport(format!("rmd {remote_path}: {e}"))
            });
        }
        enum Op {
            Visit(String),
            Remove(String),
        }
        let mut stack = vec![Op::Visit(remote_path.to_string())];
        while let Some(op) = stack.pop() {
            match op {
                Op::Visit(path) => {
                    let lines = self.stream.list(Some(&path)).await.map_err(|e| {
                        BlinkError::transport(format!("list {path}: {e}"))
                    })?;
                    stack.push(Op::Remove(path.clone()));
                    let mut subdirs: Vec<Op> = Vec::new();
                    for line in lines {
                        if line.starts_with("total ") {
                            continue;
                        }
                        let parsed = match FtpFile::from_str(&line) {
                            Ok(f) => f,
                            Err(_) => continue,
                        };
                        let name = parsed.name();
                        if name == "." || name == ".." {
                            continue;
                        }
                        let child = super::join_remote(&path, name);
                        if parsed.is_directory() {
                            subdirs.push(Op::Visit(child));
                        } else {
                            self.stream.rm(&child).await.map_err(|e| {
                                BlinkError::transport(format!("dele {child}: {e}"))
                            })?;
                        }
                    }
                    for op in subdirs.into_iter().rev() {
                        stack.push(op);
                    }
                }
                Op::Remove(path) => {
                    self.stream.rmdir(&path).await.map_err(|e| {
                        BlinkError::transport(format!("rmd {path}: {e}"))
                    })?;
                }
            }
        }
        Ok(())
    }

    async fn mkdir(&mut self, remote_path: &str) -> Result<()> {
        if let Ok(Some(existing)) = self.metadata(remote_path).await {
            if existing.is_dir() {
                return Ok(());
            }
            return Err(BlinkError::transport(format!(
                "mkdir {remote_path}: path exists and is not a directory"
            )));
        }
        self.stream
            .mkdir(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("mkd {remote_path}: {e}")))
    }

    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
        let (parent, basename) = match remote_path.rsplit_once('/') {
            Some(("", b)) => ("/".to_string(), b.to_string()),
            Some((p, b)) => (p.to_string(), b.to_string()),
            None => (".".to_string(), remote_path.to_string()),
        };

        let lines = match self.stream.list(Some(&parent)).await {
            Ok(l) => l,
            Err(_) => return Ok(None),
        };
        for line in lines {
            if line.starts_with("total ") {
                continue;
            }
            let parsed = match FtpFile::from_str(&line) {
                Ok(f) => f,
                Err(_) => continue,
            };
            if parsed.name() != basename {
                continue;
            }
            let kind = if parsed.is_directory() {
                EntryKind::Directory
            } else if parsed.is_symlink() {
                EntryKind::Symlink
            } else if parsed.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            return Ok(Some(RemoteEntry {
                name: basename,
                kind,
                size: parsed.size() as u64,
                modified: None,
                mode: None,
            }));
        }
        Ok(None)
    }

    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
        let remote_path_owned = remote_path.to_string();
        let buf = self
            .stream
            .retr(&remote_path_owned, move |reader| {
                Box::pin(async move {
                    let mut buf = Vec::new();
                    let mut limited = reader.take(MAX_PREVIEW_BYTES + 1);
                    limited
                        .read_to_end(&mut buf)
                        .await
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    let reader = limited.into_inner();
                    if buf.len() as u64 > MAX_PREVIEW_BYTES {
                        return Err(suppaftp::FtpError::ConnectionError(
                            std::io::Error::other("file exceeds preview size limit"),
                        ));
                    }
                    Ok((buf, reader))
                })
            })
            .await
            .map_err(|e| BlinkError::transport(format!("retr {remote_path}: {e}")))?;
        Ok(Bytes::from(buf))
    }

    async fn close(&mut self) -> Result<()> {
        let _ = self.stream.quit().await;
        Ok(())
    }
}

/// "Trust everything" certificate verifier for FTPS.
///
/// rustls intentionally makes this path verbose — there's no
/// `danger_accept_invalid_certs(true)` shortcut. We have to install a
/// custom `ServerCertVerifier` that returns success for every server
/// certificate AND signs off on every signature it sees.
///
/// Don't use this lightly. It defeats every protection TLS is supposed
/// to give you against MITM attacks. The session-level toggle that turns
/// this on is rendered with a red warning in the edit-session form.
mod no_verification {
    use std::sync::Arc;

    use suppaftp::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use suppaftp::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use suppaftp::rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    pub struct NoVerification;

    impl ServerCertVerifier for NoVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, suppaftp::rustls::Error>
        {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, suppaftp::rustls::Error>
        {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, suppaftp::rustls::Error>
        {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            // Listing every scheme rustls understands lets the handshake
            // proceed regardless of what the server picks.
            vec![
                SignatureScheme::RSA_PKCS1_SHA1,
                SignatureScheme::ECDSA_SHA1_Legacy,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::ED448,
            ]
        }
    }

    // Silence "unused import" if the Arc above is the only consumer.
    #[allow(dead_code)]
    fn _arc_anchor(_: Arc<NoVerification>) {}
}
