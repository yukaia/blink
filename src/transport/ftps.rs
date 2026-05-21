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

use crate::error::{BlinkError, Result};
use crate::session::{AuthMethod, Session};

use super::ftp_impl;

pub struct FtpsTransport {
    stream: AsyncRustlsFtpStream,
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
            let root_store = RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            );
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        };

        let connector =
            AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(Arc::new(config)));
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

        Ok((Self { stream }, new_pin))
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
        pub fn new(
            expected_pin: Option<String>,
            captured_pin: Arc<Mutex<Option<String>>>,
        ) -> Self {
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
    }
}
