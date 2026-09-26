//! FTPS transport — explicit TLS over the FTP control channel via rustls.
//!
//! ## Trust model
//!
//! - `accept_invalid_certs = false` (default): standard CA chain
//!   verification via webpki-roots. No pinning involved.
//! - `accept_invalid_certs = true`: CA chain trust is bypassed, but the
//!   server's certificate must still match the configured hostname
//!   (SAN/CN), and the handshake signature must verify against the
//!   cert's public key. The leaf certificate SHA-256 is pinned in the
//!   session on the first connect; subsequent connects to the same
//!   session must present the same cert. This mirrors how SSH host-key
//!   trust works for SFTP.

use std::sync::{Arc, Mutex};

use suppaftp::rustls::{ClientConfig, RootCertStore};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use zeroize::Zeroizing;

use crate::error::{BlinkError, Result};
use crate::session::{AuthMethod, Session};

use super::ftp_impl;

pub struct FtpsTransport {
    stream: AsyncRustlsFtpStream,
    /// What [`Self::reopen`] connects with, carrying the pin the first
    /// connect saw — see [`pinned_session`]. See `delegate_ftp_transport!`.
    session: Session,
    password: Option<Zeroizing<String>>,
    /// Set while a call is in flight and after one ends in `Disconnected`;
    /// the next call reconnects first. See `delegate_ftp_transport!`.
    broken: bool,
}

/// The session a reconnect uses: `session` with the certificate pin the
/// first connect captured, if it captured one. A reconnect must meet the
/// certificate that connect accepted, never trust a new one on first use:
/// the caller persists a new pin only from `connect`'s return value, so a
/// certificate first seen on a reconnect would be trusted without that.
fn pinned_session(session: &Session, new_pin: Option<&str>) -> Session {
    let mut pinned = session.clone();
    if let Some(pin) = new_pin {
        pinned.cert_sha256 = Some(pin.to_string());
    }
    pinned
}

impl FtpsTransport {
    /// Connect, perform the TLS upgrade, and log in.
    ///
    /// Returns the transport and, in the pinning (TOFU) case only, the
    /// SHA-256 of the leaf certificate so the caller can persist it on
    /// the session.
    pub async fn connect(
        session: &Session,
        password: Option<&str>,
    ) -> Result<(Self, Option<String>)> {
        let (stream, new_pin) = Self::open(session, password).await?;
        let transport = Self {
            stream,
            session: pinned_session(session, new_pin.as_deref()),
            password: password.map(|p| Zeroizing::new(p.to_string())),
            broken: false,
        };
        Ok((transport, new_pin))
    }

    /// Replace the stream with a fresh connection and login. The stored
    /// session carries a pin whenever pinning is in use, so the verifier
    /// checks against it and captures nothing new.
    async fn reopen(&mut self) -> Result<()> {
        let password = self.password.as_ref().map(|p| p.as_str());
        let (stream, _) = Self::open(&self.session, password).await?;
        self.stream = stream;
        Ok(())
    }

    async fn open(
        session: &Session,
        password: Option<&str>,
    ) -> Result<(AsyncRustlsFtpStream, Option<String>)> {
        if !matches!(session.auth, AuthMethod::Password) {
            return Err(BlinkError::auth(
                "FTPS only supports password (or anonymous) auth",
            ));
        }

        let addr = format!("{}:{}", session.host, session.port);
        let plain = AsyncRustlsFtpStream::connect(&addr)
            .await
            .map_err(|e| BlinkError::connect(format!("ftps connect to {addr}: {e}")))?;

        // Used by the pinning verifier to publish the leaf cert hash back
        // to this function after the TLS handshake completes. Only set on
        // a TOFU connect (no pin previously stored); on a pin-match connect
        // it stays None and no save is triggered.
        let captured_pin: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let config = if session.accept_invalid_certs {
            let verifier = pinning::PinningVerifier::new(
                session.cert_sha256.clone(),
                Arc::clone(&captured_pin),
            );
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        } else {
            let root_store =
                RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        let connector = AsyncRustlsConnector::from(suppaftp::tokio_rustls::TlsConnector::from(
            Arc::new(config),
        ));
        let mut stream = plain
            .into_secure(connector, &session.host)
            .await
            .map_err(|e| BlinkError::connect(format!("ftps tls upgrade: {e}")))?;

        // Handshake succeeded — extract any pin the verifier captured.
        let new_pin = captured_pin.lock().ok().and_then(|mut g| g.take());

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

        Ok((stream, new_pin))
    }
}

ftp_impl::delegate_ftp_transport!(FtpsTransport, Ftps);

/// Certificate verifier used when `accept_invalid_certs = true`.
///
/// Always:
/// - Verifies the server's hostname against the cert SAN/CN.
/// - Verifies handshake signatures against the cert's public key, using
///   the ring crypto provider already pulled in by rustls-ring.
///
/// For the leaf cert:
/// - If a pin is stored: requires an exact SHA-256 match (hex,
///   case-insensitive).
/// - If no pin is stored: captures the cert hash so the caller can
///   persist it (Trust On First Use).
mod pinning {
    use std::sync::{Arc, Mutex};

    use sha2::{Digest, Sha256};
    use suppaftp::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use suppaftp::rustls::client::verify_server_name;
    use suppaftp::rustls::crypto::{
        self, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
    };
    use suppaftp::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use suppaftp::rustls::server::ParsedCertificate;
    use suppaftp::rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

    pub struct PinningVerifier {
        expected_pin: Option<String>,
        captured_pin: Arc<Mutex<Option<String>>>,
        sig_algs: WebPkiSupportedAlgorithms,
    }

    impl std::fmt::Debug for PinningVerifier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PinningVerifier")
                .field("has_pin", &self.expected_pin.is_some())
                .finish()
        }
    }

    impl PinningVerifier {
        pub fn new(expected_pin: Option<String>, captured_pin: Arc<Mutex<Option<String>>>) -> Self {
            let sig_algs = crypto::ring::default_provider().signature_verification_algorithms;
            Self {
                expected_pin,
                captured_pin,
                sig_algs,
            }
        }
    }

    impl ServerCertVerifier for PinningVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, TlsError> {
            // Hostname binding is mandatory regardless of trust mode —
            // bypassing chain validation must not bypass the SAN check.
            let cert = ParsedCertificate::try_from(end_entity)?;
            verify_server_name(&cert, server_name)?;

            // Compute leaf cert SHA-256, hex-encoded lowercase.
            let mut hasher = Sha256::new();
            hasher.update(end_entity.as_ref());
            let hash = to_hex(&hasher.finalize());

            match &self.expected_pin {
                Some(expected) => {
                    if expected.eq_ignore_ascii_case(&hash) {
                        Ok(ServerCertVerified::assertion())
                    } else {
                        Err(TlsError::General(format!(
                            "FTPS certificate pin mismatch: stored {expected}, server presented {hash}. \
                             Edit the session to clear the pin if the change is legitimate."
                        )))
                    }
                }
                None => {
                    if let Ok(mut g) = self.captured_pin.lock() {
                        *g = Some(hash);
                    }
                    Ok(ServerCertVerified::assertion())
                }
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            verify_tls12_signature(message, cert, dss, &self.sig_algs)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, TlsError> {
            verify_tls13_signature(message, cert, dss, &self.sig_algs)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.sig_algs.supported_schemes()
        }
    }

    fn to_hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(&mut out, "{b:02x}");
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::to_hex;

        #[test]
        fn hex_lowercase() {
            assert_eq!(to_hex(&[0x00, 0xff, 0xab]), "00ffab");
        }

        #[test]
        fn hex_empty() {
            assert_eq!(to_hex(&[]), "");
        }

        /// `ClientConfig::builder()` picks rustls' crypto provider from the
        /// enabled crate features, and panics outright when both `ring` and
        /// `aws-lc-rs` are on. Nothing in the process installs a provider by
        /// hand, so exactly one backend must reach rustls — a dependency that
        /// drags in the other turns every FTPS connect into a panic.
        #[test]
        fn exactly_one_crypto_provider_reaches_rustls() {
            use suppaftp::rustls::{ClientConfig, RootCertStore};

            let _ = ClientConfig::builder()
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth();
        }

        /// blink builds `ring` only. `russh` would otherwise pull `aws-lc-rs`
        /// through its default features, and `aws-lc-sys` assembles its
        /// Windows objects with NASM — a build-time tool the Windows
        /// cross-compile has no reason to require, and did not have.
        ///
        /// The manifest turns that feature off, but a manifest is not a
        /// guarantee: cargo unifies features across the graph, so *any*
        /// dependency that enables `russh/aws-lc-rs` switches it back on for
        /// everyone. russh only refuses to compile when *neither* backend is
        /// enabled — with both, `aws-lc-rs` silently wins its `cfg` branches.
        /// That is the same shape as the `tokio-rustls` default-features bug
        /// that made every FTPS connect panic, and it fails on a platform
        /// nobody builds day to day.
        ///
        /// Reading the lockfile is the cheapest way to notice, and it fails
        /// on Linux rather than waiting for someone to cross-compile.
        #[test]
        fn aws_lc_stays_out_of_the_dependency_graph() {
            let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))
                .expect("Cargo.lock should be readable from the manifest dir");

            for crate_name in ["aws-lc-sys", "aws-lc-rs"] {
                assert!(
                    !lock.contains(&format!("name = \"{crate_name}\"")),
                    "{crate_name} is back in Cargo.lock — something enabled \
                     russh's aws-lc-rs feature. That reintroduces the NASM \
                     build requirement for x86_64-pc-windows-gnu.",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pinned_session;
    use crate::session::{AuthMethod, Protocol, Session};

    fn session(cert_sha256: Option<&str>) -> Session {
        Session {
            name: "it".to_string(),
            protocol: Protocol::Ftps,
            host: "127.0.0.1".to_string(),
            port: 990,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: true,
            cert_sha256: cert_sha256.map(str::to_string),
        }
    }

    /// Trust on first use: the pin the first connect captured is the one
    /// every reconnect must meet, so a reconnect never pins afresh.
    #[test]
    fn a_reconnect_requires_the_pin_the_first_connect_captured() {
        let pinned = pinned_session(&session(None), Some("ab12"));
        assert_eq!(pinned.cert_sha256.as_deref(), Some("ab12"));
    }

    /// A pin match captures nothing, and the stored pin carries over.
    #[test]
    fn a_reconnect_keeps_the_stored_pin() {
        let pinned = pinned_session(&session(Some("ab12")), None);
        assert_eq!(pinned.cert_sha256.as_deref(), Some("ab12"));
    }

    /// With CA verification there is no pin to carry, and none appears.
    #[test]
    fn without_pinning_a_reconnect_has_no_pin() {
        let mut ca_verified = session(None);
        ca_verified.accept_invalid_certs = false;
        let pinned = pinned_session(&ca_verified, None);
        assert_eq!(pinned.cert_sha256, None);
    }
}
