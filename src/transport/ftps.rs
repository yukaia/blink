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
use zeroize::Zeroizing;

use crate::error::{BlinkError, Result};
use crate::session::Session;
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
    let host = ftp_impl::host_port(&session.host, session.port);
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
        let plain: AsyncRustlsFtpStream = ftp_impl::open_control(session, "ftps").await?;

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

        ftp_impl::log_in(&mut stream, session, password, "ftps").await?;
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

/// End-to-end tests against an in-process FTPS server: explicit TLS on the
/// control connection, protected data connections, and just enough FTP to
/// log in and list a directory. `ftp_impl`'s server speaks the rest of the
/// protocol over plain TCP; what is FTPS's own is the certificate.
#[cfg(test)]
mod integration {
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use suppaftp::rustls::ServerConfig;
    use suppaftp::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use suppaftp::tokio_rustls::TlsAcceptor;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Mutex, mpsc};

    use super::super::ftps_test_certs as certs;
    use super::{FtpsTransport, colon_hex};
    use crate::session::{AuthMethod, Protocol, Session};
    use crate::transport::sftp::HostKeyDecision;
    use crate::transport::{NewCertPin, Transport, UserWait};
    use crate::tui::event::{AppEvent, TrustSubject};

    /// A certificate and its key, as base64 of their DER.
    type Identity = (&'static str, &'static str);
    const A: Identity = (certs::A_CERT, certs::A_KEY);
    const B: Identity = (certs::B_CERT, certs::B_KEY);
    const OTHER_NAME: Identity = (certs::OTHER_NAME_CERT, certs::OTHER_NAME_KEY);

    fn der(b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("fixture should be base64")
    }

    /// The pin of a certificate, as `cert_sha256` holds it.
    fn pin_of(identity: Identity) -> String {
        Sha256::digest(der(identity.0))
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn acceptor(identity: Identity) -> TlsAcceptor {
        let cert = CertificateDer::from(der(identity.0));
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der(identity.1)));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .expect("fixture certificate should load");
        TlsAcceptor::from(Arc::new(config))
    }

    /// The certificates a server presents, changeable while it runs.
    #[derive(Clone)]
    struct Identities {
        control: Arc<Mutex<Identity>>,
        data: Arc<Mutex<Identity>>,
    }

    /// Every command the server received, on any connection, in order.
    type Log = Arc<Mutex<Vec<String>>>;

    struct Server {
        port: u16,
        identities: Identities,
        log: Log,
    }

    impl Server {
        async fn commands(&self) -> Vec<String> {
            self.log.lock().await.clone()
        }

        /// How many times a password reached the server.
        async fn passwords(&self) -> usize {
            let log = self.log.lock().await;
            log.iter().filter(|l| l.starts_with("PASS")).count()
        }
    }

    /// Serve FTPS with `control` on the control connection and `data` on
    /// the data connections.
    async fn start_server(control: Identity, data: Identity) -> Server {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let identities = Identities {
            control: Arc::new(Mutex::new(control)),
            data: Arc::new(Mutex::new(data)),
        };
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let (identities_l, log_l) = (identities.clone(), Arc::clone(&log));
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let (identities, log) = (identities_l.clone(), Arc::clone(&log_l));
                tokio::spawn(async move {
                    // A client that refuses the certificate ends the
                    // connection mid-handshake; that is the test, not a
                    // failure of the server.
                    let _ = handle_control(sock, identities, log).await;
                });
            }
        });
        Server {
            port,
            identities,
            log,
        }
    }

    async fn handle_control(
        mut sock: TcpStream,
        identities: Identities,
        log: Log,
    ) -> std::io::Result<()> {
        sock.write_all(b"220 blink test server\r\n").await?;

        // Explicit TLS: one command in the clear, then the handshake.
        let mut first = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            if tokio::io::AsyncReadExt::read(&mut sock, &mut byte).await? == 0 {
                return Ok(());
            }
            first.push(byte[0]);
            if first.ends_with(b"\r\n") {
                break;
            }
        }
        let first = String::from_utf8_lossy(&first).trim_end().to_string();
        log.lock().await.push(first.clone());
        if first != "AUTH TLS" {
            sock.write_all(b"530 use AUTH TLS first\r\n").await?;
            return Ok(());
        }
        sock.write_all(b"234 starting TLS\r\n").await?;
        let control = *identities.control.lock().await;
        let tls = acceptor(control).accept(sock).await?;

        let (read_half, mut w) = tokio::io::split(tls);
        let mut lines = BufReader::new(read_half).lines();
        // Bound by PASV, consumed by the next LIST.
        let mut pasv: Option<TcpListener> = None;

        while let Some(line) = lines.next_line().await? {
            let line = line.trim_end();
            log.lock().await.push(line.to_string());
            let cmd = line.split(' ').next().unwrap_or("").to_ascii_uppercase();
            match cmd.as_str() {
                "PBSZ" | "PROT" | "TYPE" => w.write_all(b"200 ok\r\n").await?,
                "USER" => w.write_all(b"331 password required\r\n").await?,
                "PASS" => w.write_all(b"230 logged in\r\n").await?,
                "PASV" => {
                    let data_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
                    let port = data_listener.local_addr()?.port();
                    pasv = Some(data_listener);
                    let reply = format!(
                        "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                        port / 256,
                        port % 256
                    );
                    w.write_all(reply.as_bytes()).await?;
                }
                "LIST" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    w.write_all(b"150 here it comes\r\n").await?;
                    let (data_sock, _) = data_listener.accept().await?;
                    let data = *identities.data.lock().await;
                    match acceptor(data).accept(data_sock).await {
                        Ok(mut data_tls) => {
                            let listing = "-rw-r--r-- 1 owner group 12 Nov 01 12:00 a.txt\r\n";
                            data_tls.write_all(listing.as_bytes()).await?;
                            data_tls.shutdown().await?;
                            w.write_all(b"226 transfer complete\r\n").await?;
                        }
                        Err(_) => w.write_all(b"425 data handshake failed\r\n").await?,
                    }
                }
                "QUIT" => {
                    w.write_all(b"221 goodbye\r\n").await?;
                    break;
                }
                _ => w.write_all(b"502 command not implemented\r\n").await?,
            }
        }
        Ok(())
    }

    /// A session for the server on `port` that pins rather than checks the
    /// chain, holding `pin` if one is given.
    fn pinning_session(port: u16, pin: Option<String>) -> Session {
        Session {
            name: "it".to_string(),
            protocol: Protocol::Ftps,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: true,
            cert_sha256: pin,
        }
    }

    /// What a connect came to, and the trust prompts it raised.
    struct Attempt {
        result: crate::error::Result<(FtpsTransport, Option<NewCertPin>)>,
        /// The fingerprint each prompt showed.
        prompts: Vec<String>,
        /// Passwords the server had seen when each prompt was raised.
        passwords_at_prompt: Vec<usize>,
        /// Where later prompts would arrive, kept open so a reconnect that
        /// asked would be seen to.
        events: mpsc::UnboundedReceiver<AppEvent>,
    }

    /// Connect to `server` as `session`, answering every trust prompt with
    /// `decision`.
    async fn connect(server: &Server, session: &Session, decision: HostKeyDecision) -> Attempt {
        let (tx, mut events) = mpsc::unbounded_channel();
        let (user_wait, _waiting) = UserWait::new();
        let mut prompts = Vec::new();
        let mut passwords_at_prompt = Vec::new();

        let result = {
            let connecting = FtpsTransport::connect(session, Some("secret"), &tx, &user_wait);
            tokio::pin!(connecting);
            loop {
                tokio::select! {
                    result = &mut connecting => break result,
                    Some(event) = events.recv() => {
                        let AppEvent::HostKeyUnknown { subject, fingerprint, decision_tx, .. } = event
                        else {
                            panic!("expected a trust prompt");
                        };
                        assert_eq!(subject, TrustSubject::FtpsCertificate);
                        prompts.push(fingerprint);
                        passwords_at_prompt.push(server.passwords().await);
                        let _ = decision_tx.send(decision);
                    }
                }
            }
        };
        Attempt {
            result,
            prompts,
            passwords_at_prompt,
            events,
        }
    }

    #[tokio::test]
    async fn a_server_presenting_the_pinned_certificate_connects_and_lists() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, Some(pin_of(A)));

        let attempt = connect(&server, &session, HostKeyDecision::Reject).await;

        let (mut transport, new_pin) = attempt.result.expect("the pinned certificate connects");
        assert_eq!(new_pin, None, "a pin that matched is not a new one");
        assert!(attempt.prompts.is_empty(), "and nothing is asked");
        let listing = transport.list("/").await.expect("the data connection too");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].raw_name, "a.txt");
    }

    /// A stored pin in upper case is the same pin.
    #[tokio::test]
    async fn a_pin_matches_whatever_its_case() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, Some(pin_of(A).to_ascii_uppercase()));

        let attempt = connect(&server, &session, HostKeyDecision::Reject).await;

        assert!(attempt.result.is_ok());
    }

    #[tokio::test]
    async fn another_certificate_than_the_pinned_one_is_refused_before_the_password() {
        let server = start_server(B, B).await;
        let session = pinning_session(server.port, Some(pin_of(A)));

        let attempt = connect(&server, &session, HostKeyDecision::AcceptAndSave).await;

        let err = attempt
            .result
            .err()
            .expect("a changed certificate is refused");
        assert!(err.to_string().contains("pin mismatch"), "{err}");
        assert!(attempt.prompts.is_empty(), "a mismatch is not a question");
        assert_eq!(server.passwords().await, 0);
    }

    /// Turning chain validation off must not turn the name check off: a
    /// certificate for another host is not this host's, whoever signed it.
    #[tokio::test]
    async fn a_certificate_for_another_name_is_refused_without_asking() {
        let server = start_server(OTHER_NAME, OTHER_NAME).await;
        let session = pinning_session(server.port, None);

        let attempt = connect(&server, &session, HostKeyDecision::AcceptAndSave).await;

        assert!(attempt.result.is_err());
        assert!(
            attempt.prompts.is_empty(),
            "there is nothing to offer to trust"
        );
        assert_eq!(server.passwords().await, 0);
    }

    /// Pinning its own certificate doesn't rescue it either.
    #[tokio::test]
    async fn a_certificate_for_another_name_is_refused_even_when_pinned() {
        let server = start_server(OTHER_NAME, OTHER_NAME).await;
        let session = pinning_session(server.port, Some(pin_of(OTHER_NAME)));

        let attempt = connect(&server, &session, HostKeyDecision::AcceptAndSave).await;

        assert!(attempt.result.is_err());
        assert_eq!(server.passwords().await, 0);
    }

    #[tokio::test]
    async fn a_first_connect_asks_about_the_certificate_before_sending_the_password() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, None);

        let attempt = connect(&server, &session, HostKeyDecision::AcceptAndSave).await;

        let (_transport, new_pin) = attempt.result.expect("an accepted certificate connects");
        assert_eq!(
            new_pin,
            Some(NewCertPin {
                sha256: pin_of(A),
                save: true
            })
        );
        assert_eq!(attempt.prompts, [colon_hex(&pin_of(A))]);
        assert_eq!(attempt.passwords_at_prompt, [0], "asked first");
        assert_eq!(server.passwords().await, 1, "then logged in");
    }

    #[tokio::test]
    async fn trusting_a_certificate_once_pins_it_without_saving() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, None);

        let attempt = connect(&server, &session, HostKeyDecision::AcceptOnce).await;

        let (_transport, new_pin) = attempt.result.unwrap();
        assert_eq!(
            new_pin,
            Some(NewCertPin {
                sha256: pin_of(A),
                save: false
            })
        );
    }

    #[tokio::test]
    async fn a_rejected_certificate_never_gets_the_password() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, None);

        let attempt = connect(&server, &session, HostKeyDecision::Reject).await;

        assert!(attempt.result.is_err());
        assert_eq!(attempt.prompts.len(), 1);
        assert_eq!(server.passwords().await, 0);
        let commands = server.commands().await;
        assert!(
            !commands.iter().any(|c| c.starts_with("USER")),
            "{commands:?}"
        );
    }

    /// The data connections carry the files. One presenting another
    /// certificate than the pinned one is someone else's.
    #[tokio::test]
    async fn a_data_connection_with_another_certificate_is_refused() {
        let server = start_server(A, B).await;
        let session = pinning_session(server.port, Some(pin_of(A)));
        let attempt = connect(&server, &session, HostKeyDecision::Reject).await;
        let (mut transport, _) = attempt.result.unwrap();

        assert!(transport.list("/").await.is_err());
    }

    /// The same during the connection that first trusted a certificate,
    /// when the session itself holds no pin yet.
    #[tokio::test]
    async fn a_data_connection_must_present_the_certificate_just_accepted() {
        let server = start_server(A, B).await;
        let session = pinning_session(server.port, None);
        let attempt = connect(&server, &session, HostKeyDecision::AcceptOnce).await;
        let (mut transport, _) = attempt.result.unwrap();

        assert!(transport.list("/").await.is_err());
    }

    /// A reconnect meets a server that has changed its certificate since
    /// the user accepted the first. No one is there to ask, and the caller
    /// would not save the answer, so it is refused, password unsent.
    #[tokio::test]
    async fn a_reconnect_refuses_another_certificate_than_the_one_accepted() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, None);
        let mut attempt = connect(&server, &session, HostKeyDecision::AcceptOnce).await;
        let (mut transport, _) = attempt.result.unwrap();
        assert_eq!(server.passwords().await, 1);

        *server.identities.control.lock().await = B;
        *server.identities.data.lock().await = B;
        transport.broken = true;
        let listed = transport.list("/").await;

        let err = listed.expect_err("the reconnect is refused");
        assert!(err.to_string().contains("pin mismatch"), "{err}");
        assert!(attempt.events.try_recv().is_err(), "and nothing is asked");
        assert_eq!(server.passwords().await, 1, "nor the password sent again");
    }

    /// And one that finds the same certificate carries on.
    #[tokio::test]
    async fn a_reconnect_to_the_accepted_certificate_carries_on() {
        let server = start_server(A, A).await;
        let session = pinning_session(server.port, None);
        let mut attempt = connect(&server, &session, HostKeyDecision::AcceptOnce).await;
        let (mut transport, _) = attempt.result.unwrap();

        transport.broken = true;
        let listing = transport.list("/").await.expect("the reconnect succeeds");

        assert_eq!(listing.entries.len(), 1);
        assert!(attempt.events.try_recv().is_err(), "without asking again");
        assert_eq!(server.passwords().await, 2);
    }

    /// With chain validation on, the default, a self-signed certificate is
    /// refused outright: a pin in the session changes nothing.
    #[tokio::test]
    async fn a_self_signed_certificate_is_refused_when_the_chain_is_checked() {
        let server = start_server(A, A).await;
        let mut session = pinning_session(server.port, Some(pin_of(A)));
        session.accept_invalid_certs = false;

        let attempt = connect(&server, &session, HostKeyDecision::AcceptAndSave).await;

        assert!(attempt.result.is_err());
        assert!(attempt.prompts.is_empty());
        assert_eq!(server.passwords().await, 0);
    }

    #[tokio::test]
    async fn key_or_agent_auth_is_refused_before_connecting() {
        let server = start_server(A, A).await;
        let mut session = pinning_session(server.port, Some(pin_of(A)));
        session.auth = AuthMethod::Agent;

        let attempt = connect(&server, &session, HostKeyDecision::Reject).await;

        assert!(attempt.result.is_err());
        assert!(server.commands().await.is_empty(), "nothing was sent");
    }
}
