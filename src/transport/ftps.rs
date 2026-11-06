//! FTPS transport — explicit TLS over the FTP control channel via rustls.
//!
//! ## Trust model
//!
//! - `accept_invalid_certs = false` (default): standard CA chain
//!   verification via webpki-roots. No pinning involved.
//! - `accept_invalid_certs = true`: CA chain trust is bypassed, but the
//!   server's certificate must still match the configured hostname
//!   (SAN/CN), and the handshake signature must verify against the
//!   cert's public key. On the first connect the user is shown the leaf
//!   certificate's SHA-256 and asked, before the password is sent, whether
//!   to trust it; accepted, it is pinned in the session, and later
//!   connects (data connections included) must present the same cert.
//!   This mirrors how SSH host-key trust works for SFTP.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use suppaftp::rustls::{ClientConfig, RootCertStore};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use zeroize::Zeroizing;

use crate::error::{BlinkError, Result};
use crate::session::{AuthMethod, Session};
use crate::transport::sftp::{HOST_KEY_DECISION_TIMEOUT, HostKeyDecision};
use crate::tui::event::{AppEvent, TrustSubject};

use super::{NewCertPin, UserWait, ftp_impl};

/// Where [`FtpsTransport::open`] asks the user about a certificate the
/// session has no pin for.
struct CertPrompt<'a> {
    app_event_tx: &'a mpsc::UnboundedSender<AppEvent>,
    user_wait: &'a UserWait,
}

/// A SHA-256 in hex as `openssl x509 -fingerprint -sha256` prints it,
/// `AB:CD:…`, so the user can compare it with what they have out of band.
pub(crate) fn colon_hex(hex: &str) -> String {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

/// Ask the user whether to trust the certificate `sha256` for `session`.
///
/// The prompt is SFTP's host-key one, with the same timeout, and the connect
/// deadline paused while it's open. Reject, a timeout, or a prompt that
/// can't be shown all refuse the connection.
async fn ask_to_pin(
    session: &Session,
    sha256: &str,
    prompt: &CertPrompt<'_>,
) -> Result<NewCertPin> {
    let (decision_tx, decision_rx) = oneshot::channel();
    let host = if session.host.contains(':') {
        format!("[{}]:{}", session.host, session.port)
    } else {
        format!("{}:{}", session.host, session.port)
    };
    let event = AppEvent::HostKeyUnknown {
        subject: TrustSubject::FtpsCertificate,
        host,
        key_type: "TLS certificate, SHA-256".to_string(),
        fingerprint: colon_hex(sha256),
        decision_tx,
    };
    let rejected = || BlinkError::connect("the FTPS certificate was not accepted");
    if prompt.app_event_tx.send(event).is_err() {
        return Err(rejected());
    }
    let decision = {
        let _waiting = prompt.user_wait.waiting();
        match tokio::time::timeout(HOST_KEY_DECISION_TIMEOUT, decision_rx).await {
            Ok(d) => d.unwrap_or(HostKeyDecision::Reject),
            Err(_) => HostKeyDecision::Reject,
        }
    };
    let save = match decision {
        HostKeyDecision::AcceptAndSave => true,
        HostKeyDecision::AcceptOnce => false,
        HostKeyDecision::Reject => return Err(rejected()),
    };
    Ok(NewCertPin {
        sha256: sha256.to_string(),
        save,
    })
}

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
    /// Returns the transport and, when the session had no pin and the user
    /// accepted the certificate, that certificate's pin so the caller can
    /// keep it on the session.
    pub async fn connect(
        session: &Session,
        password: Option<&str>,
        app_event_tx: &mpsc::UnboundedSender<AppEvent>,
        user_wait: &UserWait,
    ) -> Result<(Self, Option<NewCertPin>)> {
        let prompt = CertPrompt {
            app_event_tx,
            user_wait,
        };
        let (stream, new_pin) = Self::open(session, password, Some(&prompt)).await?;
        let transport = Self {
            stream,
            session: pinned_session(session, new_pin.as_ref().map(|p| p.sha256.as_str())),
            password: password.map(|p| Zeroizing::new(p.to_string())),
            broken: false,
        };
        Ok((transport, new_pin))
    }

    /// Replace the stream with a fresh connection and login. The stored
    /// session carries a pin whenever pinning is in use, so the verifier
    /// checks against it and captures nothing new, so nothing is asked.
    async fn reopen(&mut self) -> Result<()> {
        let password = self.password.as_ref().map(|p| p.as_str());
        let (stream, _) = Self::open(&self.session, password, None).await?;
        self.stream = stream;
        Ok(())
    }

    /// `prompt` is where to ask about a certificate the session has no pin
    /// for; without one, such a certificate is refused.
    async fn open(
        session: &Session,
        password: Option<&str>,
        prompt: Option<&CertPrompt<'_>>,
    ) -> Result<(AsyncRustlsFtpStream, Option<NewCertPin>)> {
        if !matches!(session.auth, AuthMethod::Password) {
            return Err(BlinkError::auth(
                "FTPS only supports password (or anonymous) auth",
            ));
        }

        let addr = format!("{}:{}", session.host, session.port);
        let mut plain = AsyncRustlsFtpStream::connect(&addr)
            .await
            .map_err(|e| BlinkError::connect(format!("ftps connect to {addr}: {e}")))?;

        // Dial the data connection at the control connection's peer, on the
        // port the PASV reply gives, not at the address the reply names. The
        // server picks that address, so a hostile one could aim uploads at
        // any host it likes, such as a service on the user's own network;
        // NAT'd servers that advertise a private address need this anyway.
        // curl has ignored the PASV address by default since 7.74. The
        // setting carries through `into_secure` below.
        plain.set_passive_nat_workaround(true);

        // The pinning verifier records here the first leaf certificate it
        // sees when the session has no pin, and holds every later
        // handshake on this stream (the data connections) to it. On a
        // connect with a pin it stays None and nothing is asked.
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

        // Handshake succeeded. A certificate the session had no pin for
        // is the user's call, and must be made before the password goes
        // to whoever holds it.
        let captured = captured_pin.lock().ok().and_then(|g| g.clone());
        let new_pin = match (captured, prompt) {
            (None, _) => None,
            (Some(sha256), Some(prompt)) => Some(ask_to_pin(session, &sha256, prompt).await?),
            (Some(_), None) => {
                return Err(BlinkError::connect(
                    "the FTPS server presented a certificate the session has no pin for",
                ));
            }
        };

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
/// - If no pin is stored: records the first cert's hash so the caller can
///   ask the user about it (Trust On First Use), and requires every later
///   handshake through this verifier, which the data connections share, to
///   present that same cert. See [`pinning::check_pin`].
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

            check_pin(self.expected_pin.as_deref(), &self.captured_pin, hash)?;
            Ok(ServerCertVerified::assertion())
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

    /// The pin half of [`PinningVerifier::verify_server_cert`], given the
    /// leaf certificate's hex SHA-256.
    ///
    /// With a stored pin, the certificate must match it. Without one, the
    /// first certificate is recorded in `first_seen` for the caller to ask
    /// about, and each later one must match it: the verifier also checks
    /// the data connections, which during a first connect would otherwise
    /// accept any certificate at all.
    pub(super) fn check_pin(
        expected: Option<&str>,
        first_seen: &Mutex<Option<String>>,
        hash: String,
    ) -> Result<(), TlsError> {
        let mismatch = |pinned: &str| {
            TlsError::General(format!(
                "FTPS certificate pin mismatch: stored {pinned}, server presented {hash}. \
                 Edit the session to clear the pin if the change is legitimate."
            ))
        };
        if let Some(expected) = expected {
            return if expected.eq_ignore_ascii_case(&hash) {
                Ok(())
            } else {
                Err(mismatch(expected))
            };
        }
        let mut seen = first_seen
            .lock()
            .map_err(|_| TlsError::General("FTPS pin state is poisoned".to_string()))?;
        match seen.as_deref() {
            Some(first) if first != hash => Err(mismatch(first)),
            Some(_) => Ok(()),
            None => {
                *seen = Some(hash);
                Ok(())
            }
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
        use super::{check_pin, to_hex};
        use std::sync::Mutex;

        #[test]
        fn a_stored_pin_must_match() {
            let seen = Mutex::new(None);
            assert!(check_pin(Some("AB"), &seen, "ab".into()).is_ok());
            assert!(check_pin(Some("ab"), &seen, "cd".into()).is_err());
            assert!(seen.lock().unwrap().is_none(), "nothing to ask about");
        }

        /// The verifier also checks the data connections; during the first
        /// connect they must present the certificate the user is asked
        /// about, not any certificate at all.
        #[test]
        fn without_a_pin_later_handshakes_must_match_the_first() {
            let seen = Mutex::new(None);
            assert!(check_pin(None, &seen, "ab".into()).is_ok());
            assert_eq!(seen.lock().unwrap().as_deref(), Some("ab"));
            assert!(check_pin(None, &seen, "ab".into()).is_ok());
            assert!(check_pin(None, &seen, "cd".into()).is_err());
            assert_eq!(seen.lock().unwrap().as_deref(), Some("ab"));
        }

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
    use super::{CertPrompt, ask_to_pin, colon_hex, pinned_session};
    use crate::session::{AuthMethod, Protocol, Session};
    use crate::transport::sftp::HostKeyDecision;
    use crate::transport::{NewCertPin, UserWait};
    use crate::tui::event::{AppEvent, TrustSubject};

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

    #[test]
    fn a_fingerprint_reads_as_openssl_prints_it() {
        assert_eq!(colon_hex("00ffab"), "00:FF:AB");
        assert_eq!(colon_hex(&"a".repeat(64)).len(), 95);
    }

    /// Ask about `sha256` and answer with `decision`, as the TUI would;
    /// `None` drops the prompt unanswered.
    async fn answer_prompt(decision: Option<HostKeyDecision>) -> crate::error::Result<NewCertPin> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (user_wait, _waiting) = UserWait::new();
        let answer = tokio::spawn(async move {
            let Some(AppEvent::HostKeyUnknown {
                subject,
                host,
                fingerprint,
                decision_tx,
                ..
            }) = rx.recv().await
            else {
                panic!("expected a prompt");
            };
            assert_eq!(subject, TrustSubject::FtpsCertificate);
            assert_eq!(host, "127.0.0.1:990");
            assert_eq!(fingerprint, "AB:CD");
            if let Some(d) = decision {
                let _ = decision_tx.send(d);
            }
        });
        let prompt = CertPrompt {
            app_event_tx: &tx,
            user_wait: &user_wait,
        };
        let out = ask_to_pin(&session(None), "abcd", &prompt).await;
        answer.await.unwrap();
        out
    }

    #[tokio::test]
    async fn accepting_and_saving_pins_and_saves() {
        let pin = answer_prompt(Some(HostKeyDecision::AcceptAndSave))
            .await
            .unwrap();
        assert_eq!(
            pin,
            NewCertPin {
                sha256: "abcd".into(),
                save: true
            }
        );
    }

    #[tokio::test]
    async fn trusting_once_pins_without_saving() {
        let pin = answer_prompt(Some(HostKeyDecision::AcceptOnce))
            .await
            .unwrap();
        assert!(!pin.save);
    }

    #[tokio::test]
    async fn a_rejected_or_unanswered_certificate_refuses_the_connection() {
        assert!(answer_prompt(Some(HostKeyDecision::Reject)).await.is_err());
        assert!(answer_prompt(None).await.is_err());
    }
}
