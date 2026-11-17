//! SFTP transport built on russh + russh-sftp.
//!
//! NOTE: russh and russh-sftp evolve their APIs across minor versions. The
//! shape below targets `russh` 0.65.x and `russh-sftp` 3.0.x. If a `cargo build`
//! reports method-not-found errors here, check the exact constructor / method
//! names against the version actually pulled in by `Cargo.lock`. The trait
//! interface in `transport::Transport` is stable; only this file should need
//! tweaking.
//!
//! Things that moved in the 0.49 → 0.60 jump, for whoever does the next one:
//! `Handler` uses native `async fn` instead of `#[async_trait]`; the
//! `authenticate_*` calls return `AuthResult` rather than `bool`;
//! `authenticate_publickey_with` takes an explicit `hash_alg`; agent
//! identities are `AgentIdentity` (key *or* certificate) rather than bare
//! `PublicKey`; and `PrivateKeyWithHashAlg::new` is infallible.
//!
//! And in the 0.60 → 0.63 jump: `check_server_key` receives a
//! `PublicKeyOrCertificate` rather than a `PublicKey`; the server-side
//! `channel_open_*` callbacks return `()` and take a `ChannelOpenHandle` that
//! must be accepted explicitly (dropping it rejects the channel); and russh
//! dropped its `internal-russh-forked-ssh-key` fork for upstream `ssh-key`.
//!
//! The 0.63 → 0.64 jump needed nothing here. It removed the
//! `legacy-ed25519-pkcs8-parser` feature, which blink never enabled, and
//! added host-key rotation (`hostkeys-00@openssh.com`) support that blink
//! does not use: `known_hosts` learns keys only through the prompt.
//!
//! The 0.64 → 0.65 jump touched only the test server: its
//! `auth_publickey` receives a `server::VerifiedPublicKey` rather than a
//! `&PublicKey`. On the client side `kex_done` now runs after
//! `check_server_key` on the first key exchange, and a signer that fails
//! (an agent declining to sign) no longer leaves the handle stuck. 0.65 also
//! added agent session binding (`Handle::agent_session_binding`), which
//! blink does not use.

use crate::transfer::MAX_QUEUED_JOBS;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use russh::client::{self, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::ssh_key;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};

use crate::error::{self, BlinkError, Result};
use crate::known_hosts::{self, KeyStatus, SessionTrust};
use crate::session::{AuthMethod, Protocol, Session};
use crate::transport::error_map::map_sftp;
use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry, Transport};

/// Cap on bytes read by `read_to_bytes`.
///
/// Derived from the viewer's image limit rather than restating it, so raising
/// one can't leave the other behind — this is the backstop for a server that
/// under-reports a file's size in its listing, and if it were the smaller of
/// the two, images between the two values would truncate instead of being
/// refused.
pub(crate) const MAX_PREVIEW_BYTES: u64 = crate::preview::IMAGE_VIEW_LIMIT;

// ---------------------------------------------------------------------------
// Host-key decision types (shared with the TUI layer via AppEvent)
// ---------------------------------------------------------------------------

/// The user's response to an unknown host-key prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Accept the key and save it to the known-hosts file.
    AcceptAndSave,
    /// Accept the key for this session only; do not persist.
    AcceptOnce,
    /// Reject the key; abort the connection.
    Reject,
}

// ---------------------------------------------------------------------------
// russh client handler
// ---------------------------------------------------------------------------

/// SSH client handler that enforces the known-hosts policy:
///
/// - If the host+key is in the known-hosts file → accept.
/// - If the host has a *different* key on file → hard reject (possible MITM).
/// - If the host is unknown → send a prompt event to the TUI and wait for the
///   user's decision on `decision_rx`.
struct KnownHostsHandler {
    /// Hostname used for known-hosts lookup. Stored as the user typed it;
    /// `known_hosts` does its own case-folding.
    host: String,
    /// Port the user connected to; combined with `host` to form the
    /// known-hosts lookup key (`host` for port 22, `[host]:port` otherwise).
    port: u16,
    /// Sends unknown-key info to the TUI so a confirmation modal can appear.
    event_tx: Option<mpsc::UnboundedSender<crate::tui::event::AppEvent>>,
    /// Keys the user accepted for this session without saving them. Shared
    /// with every other connection the session opens, so an accept-once is
    /// answered once rather than per worker. See [`SessionTrust`].
    trust: SessionTrust,
    /// Marks the time the prompt waits on the user, so the caller's connect
    /// deadline does not run meanwhile. See [`crate::transport::UserWait`].
    user_wait: crate::transport::UserWait,
}

/// How long the host-key prompt waits for an answer before rejecting.
///
/// The connect deadline is paused meanwhile, so this is the user's whole
/// allowance. Two minutes is OpenSSH's default `LoginGraceTime`: the server
/// drops an unauthenticated connection after that, so waiting longer would
/// only trade an honest "prompt timed out" for a confusing failure after the
/// user finally answers.
pub(crate) const HOST_KEY_DECISION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(120);

impl KnownHostsHandler {
    /// Human-readable form of the host, used in TUI prompts and log lines.
    /// Matches the canonical known-hosts form for consistency.
    fn display_host(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        }
    }
}

// russh 0.60 declares `Handler` with native `async fn` (RPITIT) rather than
// `#[async_trait]`, so this impl must not be wrapped in the macro.
impl Handler for KnownHostsHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        presented: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // blink has no @cert-authority support: known_hosts stores a host
        // against a literal key, so there is nothing to validate a
        // certificate's CA signature, principals, or validity window against.
        // Pinning one by its key would look like verification while checking
        // none of that, so refuse — fail closed, as the known-hosts read error
        // below does. russh 0.60 could not surface a certificate to this
        // callback, so nothing that works today starts failing.
        let server_public_key = match presented {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                tracing::warn!(
                    host = %self.display_host(),
                    "server presented a host certificate — rejecting: \
                     blink does not support host certificates",
                );
                // Send the rejection event so the TUI states the real reason
                // rather than a generic connect failure. Everything here is
                // derived from fields already held, so the arm still returns
                // before any known_hosts read — the reject cannot depend on
                // the filesystem.
                let event = crate::tui::event::AppEvent::HostCertificateRejected {
                    host: self.display_host(),
                };
                if let Some(tx) = self.event_tx.take() {
                    let _ = tx.send(event);
                }
                return Ok(false);
            }
        };

        // Sanitize before any use: a server claiming a non-standard algorithm
        // name could inject ANSI sequences into the host-key modal and log.
        let key_type = error::sanitize(server_public_key.algorithm().as_str().to_string());
        // Fail closed, as the known-hosts read error below does.
        let Some(key_b64) = host_key_b64(server_public_key.to_bytes()) else {
            tracing::error!(
                host = %self.display_host(),
                "the server's host key could not be encoded — rejecting connection",
            );
            return Ok(false);
        };

        let fingerprint = error::sanitize(
            server_public_key
                .fingerprint(ssh_key::HashAlg::Sha256)
                .to_string(),
        );

        let display_host = self.display_host();

        // Not in the file, but the user may already have accepted a key for
        // this host this session. Every worker connection lands here, and
        // re-asking would pop a modal mid-transfer for each one. That key
        // vouches for the host as a stored one does, so another is changed.
        let (status, trusted_once) =
            match known_hosts::check(&self.host, self.port, &key_type, &key_b64) {
                Ok(KeyStatus::Unknown) => (
                    self.trust.check(&self.host, self.port, &key_type, &key_b64),
                    true,
                ),
                Ok(status) => (status, false),
                Err(e) => {
                    // Fail closed: if we cannot read the known-hosts file we
                    // cannot verify the host key, so reject the connection
                    // rather than prompting the user (which would be
                    // fail-open).
                    tracing::error!(
                        host = %display_host,
                        "known_hosts read error — rejecting connection: {e}"
                    );
                    return Ok(false);
                }
            };

        match status {
            KeyStatus::Trusted => return Ok(true),
            KeyStatus::Changed {
                stored_key_type, ..
            } => {
                tracing::warn!(
                    host = %display_host,
                    stored = %stored_key_type,
                    presented = %key_type,
                    trusted_once,
                    "host key mismatch — rejecting connection",
                );
                // Send the changed-key event so the TUI can surface a clear
                // error message rather than a generic connect failure.
                let event = crate::tui::event::AppEvent::HostKeyChanged {
                    host: display_host,
                    lookup_host: self.host.clone(),
                    lookup_port: self.port,
                    stored_key_type,
                    presented_key_type: key_type,
                    fingerprint,
                    trusted_once,
                };
                if let Some(tx) = self.event_tx.take() {
                    let _ = tx.send(event);
                }
                return Ok(false);
            }
            KeyStatus::Revoked => {
                tracing::warn!(
                    host = %display_host,
                    key_type = %key_type,
                    %fingerprint,
                    "host key is revoked in known_hosts — rejecting connection",
                );
                let event = crate::tui::event::AppEvent::HostKeyRevoked {
                    host: display_host,
                    key_type,
                    fingerprint,
                };
                if let Some(tx) = self.event_tx.take() {
                    let _ = tx.send(event);
                }
                return Ok(false);
            }
            KeyStatus::Unknown => {}
        }

        // Unknown key: send the details to the TUI and await the user's call.
        let (decision_tx, decision_rx) = oneshot::channel();

        let event = crate::tui::event::AppEvent::HostKeyUnknown {
            subject: crate::tui::event::TrustSubject::SshHostKey,
            host: display_host.clone(),
            key_type: key_type.clone(),
            fingerprint,
            decision_tx,
        };

        let tx = match self.event_tx.take() {
            Some(tx) => tx,
            None => return Ok(false),
        };
        if tx.send(event).is_err() {
            return Ok(false);
        }

        // Wait for the user, with the connect deadline paused, but not
        // forever: see `HOST_KEY_DECISION_TIMEOUT`.
        let decision = {
            let _waiting = self.user_wait.waiting();
            match tokio::time::timeout(HOST_KEY_DECISION_TIMEOUT, decision_rx).await {
                Ok(d) => d.unwrap_or(HostKeyDecision::Reject),
                Err(_) => {
                    tracing::warn!(
                        host = %self.host,
                        "host-key decision timed out — rejecting"
                    );
                    HostKeyDecision::Reject
                }
            }
        };

        match decision {
            HostKeyDecision::AcceptAndSave => {
                if let Err(e) = known_hosts::append(&self.host, self.port, &key_type, &key_b64) {
                    tracing::warn!("could not save host key: {e}");
                }
                Ok(true)
            }
            HostKeyDecision::AcceptOnce => {
                // Scope the decision to this session so the connections that
                // follow — one per transfer worker — don't ask again.
                self.trust.trust(&self.host, self.port, &key_type, &key_b64);
                Ok(true)
            }
            HostKeyDecision::Reject => Ok(false),
        }
    }
}

/// A host key's wire encoding in base64, as `known_hosts` stores it, or
/// `None` for a key that did not encode. Such a key has nothing to compare
/// or store, and standing the empty string in for it would let the user
/// trust a blob any other key that failed the same way matches.
fn host_key_b64<E>(wire: std::result::Result<Vec<u8>, E>) -> Option<String> {
    use base64::Engine as _;
    let wire = wire.ok().filter(|w| !w.is_empty())?;
    Some(base64::engine::general_purpose::STANDARD.encode(wire))
}

/// The host-key algorithms to offer, those of a type `known_hosts` already
/// holds for this host first.
///
/// The client's order decides which of its keys a server proves itself
/// with. Asking for a stored type first means a server with several keys
/// presents one blink can check, so `known_hosts::check` can treat any
/// other type as a changed key without rejecting honest servers. This is
/// OpenSSH's behaviour too. The rest of the default list follows in its own
/// order, so a host seen for the first time negotiates as before.
fn host_key_preference(stored_types: &[String]) -> Vec<ssh_key::Algorithm> {
    // Stored types are key types, as `PublicKey::algorithm` names them; the
    // three RSA signature algorithms all verify an `ssh-rsa` key.
    let key_type = |alg: &ssh_key::Algorithm| match alg {
        ssh_key::Algorithm::Rsa { .. } => "ssh-rsa".to_string(),
        other => other.as_str().to_string(),
    };
    let (mut known, rest): (Vec<_>, Vec<_>) = russh::Preferred::default()
        .key
        .iter()
        .cloned()
        .partition(|alg| stored_types.contains(&key_type(alg)));
    known.extend(rest);
    known
}

/// Hash algorithm to request for a public-key authentication attempt.
///
/// RSA keys must ask for `rsa-sha2-512` explicitly: OpenSSH 8.8+ (Sept 2021)
/// disables `ssh-rsa` (SHA-1) by default, so leaving this `None` silently
/// fails against modern servers. Other key types have no hash to choose
/// (Ed25519 in particular) and take `None`.
fn rsa_hash_alg(algorithm: &ssh_key::Algorithm) -> Option<ssh_key::HashAlg> {
    matches!(algorithm, ssh_key::Algorithm::Rsa { .. }).then_some(ssh_key::HashAlg::Sha512)
}

/// What to suggest when the agent holds no keys.
#[cfg(windows)]
const AGENT_EMPTY_HINT: &str = "try `ssh-add` or load keys into Pageant";
#[cfg(not(windows))]
const AGENT_EMPTY_HINT: &str = "try `ssh-add`";

/// Offer every identity in the agent until the server accepts one.
///
/// Shared by every platform's agent, so a failure reads the same wherever
/// blink runs: the message says whether the agent was empty or every key
/// was refused, and carries the last error a key's attempt raised. Errors
/// come back as the message alone so a caller can add where the agent was
/// found. Generic over the stream rather than `Box<dyn AgentStream>` so
/// the compiler can check the `Send` bound on every path.
#[cfg(any(unix, windows))]
async fn try_agent_identities<H, S>(
    handle: &mut Handle<H>,
    username: &str,
    agent: &mut russh::keys::agent::client::AgentClient<S>,
) -> std::result::Result<(), String>
where
    H: Handler,
    S: russh::keys::agent::client::AgentStream
        + tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + Unpin
        + Send
        + 'static,
{
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| format!("ssh-agent request_identities: {e}"))?;
    if identities.is_empty() {
        return Err(format!(
            "ssh-agent has no identities loaded ({AGENT_EMPTY_HINT})"
        ));
    }

    let mut last_err: Option<String> = None;
    for identity in identities {
        // russh 0.60 models agent identities as plain keys OR OpenSSH
        // certificates; `public_key` normalises both.
        let pubkey = identity.public_key().into_owned();
        let hash_alg = rsa_hash_alg(&pubkey.algorithm());
        match handle
            .authenticate_publickey_with(username, pubkey, hash_alg, agent)
            .await
        {
            Ok(r) if r.success() => return Ok(()),
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("ssh-agent identity rejected: {e}");
                last_err = Some(e.to_string());
            }
        }
        // A server that counts attempts (OpenSSH's MaxAuthTries) hangs up
        // once they run out; every later key would only fail the same way.
        if handle.is_closed() {
            break;
        }
    }
    Err(format!(
        "ssh-agent: no identity accepted{}",
        last_err
            .map(|e| format!(" (last error: {e})"))
            .unwrap_or_default()
    ))
}

pub struct SftpTransport {
    handle: Handle<KnownHostsHandler>,
    sftp: SftpSession,
    /// Lazily-opened second SFTP channel used solely for pipelined byte
    /// transfers; all control ops stay on `sftp`. Opened on the first
    /// download/upload and reused for the connection's lifetime.
    transfer: Option<Transfer>,
    /// Where downloads' bytes come from, recorded in their resume sidecars.
    /// See [`super::origin_of`].
    origin: String,
}

/// A dedicated SFTP session for bulk transfers, plus the per-direction request
/// sizes negotiated with the server and which extensions it supports.
struct Transfer {
    raw: Arc<RawSftpSession>,
    read_chunk: usize,
    write_chunk: usize,
    fsync: bool,
    /// Server supports `posix-rename@openssh.com` (rename that atomically
    /// replaces an existing target). Used to finalize uploads.
    posix_rename: bool,
}

/// Maximum bytes per SFTP read/write request. Matches russh-sftp's own default
/// cap (255 KiB); a smaller negotiated server limit clamps it further.
const TRANSFER_CHUNK: usize = 261_120;

/// Number of SFTP read/write requests kept in flight at once. A single SFTP
/// stream without pipelining is RTT-bound — one request per network
/// round-trip — so throughput collapses on anything but a LAN. Overlapping N
/// requests makes single-stream transfers bandwidth-bound instead, the way
/// OpenSSH's own sftp client does.
const TRANSFER_CONCURRENCY: usize = 16;

/// Advertised SSH channel receive window. Must comfortably exceed
/// `TRANSFER_CHUNK * TRANSFER_CONCURRENCY` (~4 MiB) so pipelined downloads
/// aren't throttled by flow control before the requests can overlap. This is
/// an upper bound on in-flight data, not a preallocation.
const CHANNEL_WINDOW: u32 = 16 * 1024 * 1024;

/// SSH-layer keepalive interval. The russh session sends a global request
/// at this cadence; if the peer drops the connection or the network
/// silently dies, the next [`KEEPALIVE_MAX`] unanswered probes
/// (~`interval × max`) tear the session down with an error instead of
/// pinning the worker indefinitely.
///
/// Without this, only `connect+auth` had a deadline — a stalled `read_dir`
/// or `read` mid-walk would wait on the underlying TCP keepalive (often
/// many minutes on Linux) before erroring.
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const KEEPALIVE_MAX: usize = 3;

/// How long a transfer may go without a reply before it counts as lost: a
/// minute, as FTP's data connection gets.
///
/// This measures silence, not how long one request takes. russh-sftp's own
/// per-request limit starts its clock when a request is sent, and the
/// pipeline keeps [`TRANSFER_CONCURRENCY`] of them queued. On a slow link a
/// request can wait in line longer than any fixed limit while data arrives
/// the whole time, so that limit is raised out of the way (see
/// [`transfer_request_timeout`]) and this one guards every call instead.
const SFTP_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// [`SFTP_IDLE_TIMEOUT`], except where a test has shortened it for its own
/// thread, as FTP's `op_timeout` does.
#[cfg(not(test))]
fn idle_timeout() -> std::time::Duration {
    SFTP_IDLE_TIMEOUT
}

#[cfg(test)]
thread_local! {
    static IDLE_TIMEOUT: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(SFTP_IDLE_TIMEOUT) };
}

#[cfg(test)]
fn idle_timeout() -> std::time::Duration {
    IDLE_TIMEOUT.get()
}

/// russh-sftp's per-request limit for the transfer session, in seconds.
///
/// A request waits behind at most [`TRANSFER_CONCURRENCY`] others, and each
/// of those finishes within [`idle_timeout`] of the one before or the
/// transfer has already failed. So a request still waiting after that many
/// idle periods is past anything a working connection does, and this limit
/// never decides a live transfer.
fn transfer_request_timeout() -> u64 {
    let periods = TRANSFER_CONCURRENCY as u32 + 1;
    (idle_timeout() * periods).as_secs().max(1)
}

/// Run one transfer request under [`idle_timeout`]. Silence past it reads as
/// a lost connection, since the transfer's state is then unknown: the
/// dispatcher closes the connection after a failed job rather than pooling
/// it.
async fn within_idle<T>(
    op: &str,
    path: &str,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let idle = idle_timeout();
    tokio::time::timeout(idle, fut).await.unwrap_or_else(|_| {
        Err(BlinkError::disconnected(format!(
            "{op} {path}: no reply for {}s",
            idle.as_secs_f32(),
        )))
    })
}

/// Load the private key at `path`, with `passphrase` if one was given.
///
/// [`BlinkError::KeyNeedsPassphrase`] when the key is encrypted and either
/// no passphrase was given or the one given didn't decrypt it, so the TUI
/// asks (again); a load error otherwise.
fn load_key(path: &Path, passphrase: Option<&str>) -> Result<russh::keys::PrivateKey> {
    let load_error =
        |e: &dyn std::fmt::Display| BlinkError::auth(format!("load key {}: {e}", path.display()));
    let text = std::fs::read_to_string(path).map_err(|e| load_error(&e))?;
    russh::keys::decode_secret_key(&text, passphrase).map_err(|e| {
        if key_needs_passphrase(&e, &text, passphrase.is_some()) {
            BlinkError::KeyNeedsPassphrase
        } else {
            load_error(&e)
        }
    })
}

/// Whether `err`, from decoding the key `text`, means a passphrase is
/// missing or wrong.
///
/// By error variant rather than message text, which upstream can reword.
/// The variants, per format, as russh 0.65 reports them:
///
/// | format                  | no passphrase        | wrong passphrase            |
/// |-------------------------|----------------------|-----------------------------|
/// | OpenSSH                 | `KeyIsEncrypted`     | `SshKey(Crypto)`            |
/// | legacy PEM (`Proc-Type`)| `KeyIsEncrypted`     | `Unpad(_)`                  |
/// | PKCS#8 (`ENCRYPTED …`)  | a DER parse error    | `Pkcs8(EncryptedPrivateKey)`|
///
/// russh doesn't recognise an encrypted PKCS#8 key as encrypted, and parses
/// it as a plain one, so for that format the PEM label decides. The
/// wrong-passphrase variants count only when a passphrase was given: from
/// an unencrypted key they would mean corruption, not a missing secret.
/// `sftp_test_keys` holds one encrypted key per format to pin this table.
fn key_needs_passphrase(err: &russh::keys::Error, text: &str, passphrase_given: bool) -> bool {
    use russh::keys::Error as E;
    let pkcs8_encrypted = text
        .trim_start()
        .starts_with("-----BEGIN ENCRYPTED PRIVATE KEY-----");
    match err {
        E::KeyIsEncrypted => true,
        E::SshKey(ssh_key::Error::Crypto) | E::Unpad(_) => passphrase_given,
        E::Pkcs8(_) if pkcs8_encrypted => passphrase_given,
        _ => pkcs8_encrypted && !passphrase_given,
    }
}

impl SftpTransport {
    pub async fn connect(
        session: &Session,
        password: Option<&str>,
        app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
        trust: SessionTrust,
        user_wait: crate::transport::UserWait,
    ) -> Result<Self> {
        let stored_types = known_hosts::stored_key_types(&session.host, session.port);
        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE_INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            window_size: CHANNEL_WINDOW,
            preferred: russh::Preferred {
                key: host_key_preference(&stored_types).into(),
                ..russh::Preferred::default()
            },
            ..client::Config::default()
        });
        let addr = format!("{}:{}", session.host, session.port);

        let handler = KnownHostsHandler {
            host: session.host.clone(),
            port: session.port,
            event_tx: Some(app_event_tx),
            trust,
            user_wait,
        };

        let mut handle = client::connect(config, addr.clone(), handler)
            .await
            .map_err(|e| BlinkError::connect(format!("ssh connect to {addr}: {e}")))?;

        // ---- Authenticate ----
        let username = &session.username;
        let auth_result = match &session.auth {
            AuthMethod::Password => {
                let pw = password
                    .ok_or_else(|| BlinkError::auth("password required but none provided"))?;
                handle
                    .authenticate_password(username, pw)
                    .await
                    .map_err(|e| BlinkError::auth(e.to_string()))?
                    .success()
            }
            AuthMethod::Key { path } => {
                let passphrase = password.filter(|p| !p.is_empty());
                let kp = load_key(path, passphrase)?;
                let hash_alg = rsa_hash_alg(&kp.algorithm());
                let kp = PrivateKeyWithHashAlg::new(Arc::new(kp), hash_alg);
                handle
                    .authenticate_publickey(username, kp)
                    .await
                    .map_err(|e| BlinkError::auth(e.to_string()))?
                    .success()
            }
            AuthMethod::Agent => {
                #[cfg(unix)]
                {
                    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
                        .await
                        .map_err(|e| BlinkError::auth(format!("ssh-agent connect: {e}")))?;
                    try_agent_identities(&mut handle, username, &mut agent)
                        .await
                        .map_err(BlinkError::auth)?;
                    true
                }
                #[cfg(windows)]
                {
                    use russh::keys::agent::client::AgentClient;

                    const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

                    match AgentClient::connect_named_pipe(OPENSSH_PIPE).await {
                        Ok(mut agent) => try_agent_identities(&mut handle, username, &mut agent)
                            .await
                            .map_err(BlinkError::auth)?,
                        Err(pipe_err) => {
                            // russh 0.60 made connect_pageant fallible;
                            // it previously returned the client directly.
                            let mut agent = AgentClient::connect_pageant().await.map_err(|e| {
                                BlinkError::auth(format!(
                                    "ssh-agent: no agent found (OpenSSH pipe error: \
                                     {pipe_err}; Pageant error: {e})"
                                ))
                            })?;
                            // Pageant answered, so the failure is its keys'.
                            // Name it, and why the OpenSSH agent wasn't used.
                            try_agent_identities(&mut handle, username, &mut agent)
                                .await
                                .map_err(|e| {
                                    BlinkError::auth(format!(
                                        "{e} (via Pageant; OpenSSH pipe error: {pipe_err})"
                                    ))
                                })?
                        }
                    }
                    true
                }
                #[cfg(not(any(unix, windows)))]
                {
                    return Err(BlinkError::auth(
                        "ssh-agent auth is not supported on this platform",
                    ));
                }
            }
        };

        if !auth_result {
            return Err(BlinkError::auth("rejected by server"));
        }

        // ---- Open SFTP subsystem ----
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| BlinkError::transport(format!("open session: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| BlinkError::transport(format!("request sftp: {e}")))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| BlinkError::transport(format!("init sftp: {e}")))?;

        Ok(Self {
            handle,
            sftp,
            transfer: None,
            origin: super::origin_of(session),
        })
    }

    /// Open (once) and return the dedicated transfer session. A second SFTP
    /// channel is used so the high-level `sftp` session keeps serving control
    /// ops untouched, while bulk transfers drive `RawSftpSession` directly —
    /// the only way to issue the concurrent, pipelined reads/writes that make
    /// a single stream fast.
    async fn transfer_session(&mut self) -> Result<&Transfer> {
        if self.transfer.is_none() {
            let channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|e| BlinkError::transport(format!("open transfer session: {e}")))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| BlinkError::transport(format!("request sftp: {e}")))?;
            let mut raw = RawSftpSession::new(channel.into_stream());
            let version = raw
                .init()
                .await
                .map_err(|e| BlinkError::transport(format!("init sftp: {e}")))?;

            let fsync = version
                .extensions
                .get("fsync@openssh.com")
                .is_some_and(|v| v == "1");
            let posix_rename = version
                .extensions
                .get("posix-rename@openssh.com")
                .is_some_and(|v| v == "1");

            // Clamp our request size to whatever the server advertises so a
            // peer with tighter limits than the 255 KiB default doesn't reject
            // oversized reads/writes.
            let mut read_chunk = TRANSFER_CHUNK;
            let mut write_chunk = TRANSFER_CHUNK;
            if version
                .extensions
                .get("limits@openssh.com")
                .is_some_and(|v| v == "1")
                && let Ok(ext) = raw.limits().await
            {
                let limits = Limits::from(ext);
                if let Some(r) = limits.read_len {
                    read_chunk = read_chunk.min(r as usize);
                }
                if let Some(w) = limits.write_len {
                    write_chunk = write_chunk.min(w as usize);
                }
                raw.set_limits(limits);
            }
            // Only once the session is up: until then the default limit
            // bounds the handshake, which nothing else does.
            raw.set_timeout(transfer_request_timeout());

            self.transfer = Some(Transfer {
                raw: Arc::new(raw),
                read_chunk: read_chunk.max(1),
                write_chunk: write_chunk.max(1),
                fsync,
                posix_rename,
            });
        }
        Ok(self.transfer.as_ref().unwrap())
    }

    /// `delete_dir` with the pending-work ceiling spelled out, so the guard
    /// can be exercised without standing up a tree of `MAX_QUEUED_JOBS`
    /// entries.
    async fn delete_dir_capped(
        &mut self,
        remote_path: &str,
        recursive: bool,
        limit: usize,
    ) -> Result<()> {
        if !recursive {
            return self
                .sftp
                .remove_dir(remote_path)
                .await
                .map_err(|e| map_sftp("rmdir", remote_path, e));
        }

        enum Op {
            Visit(String),
            Remove(String),
        }

        let mut stack = vec![Op::Visit(remote_path.to_string())];
        while let Some(op) = stack.pop() {
            // Same ceiling, and the same reason, as `walk_remote`: a server
            // is free to serve a tree deeper or wider than this process can
            // hold, and the stack is what grows with it. Checked after the
            // pop so the count is the work still outstanding, matching the
            // walk.
            if stack.len() > limit {
                return Err(BlinkError::transport(format!(
                    "recursive delete exceeded {limit} entries — \
                     narrow the target or remove it in smaller parts",
                )));
            }
            match op {
                Op::Visit(path) => {
                    // Only descend into what is a directory *now*, by lstat.
                    // The root's kind came from a listing the user may have
                    // read minutes ago, and a subdirectory's from its
                    // parent's listing; one swapped for a symlink since would
                    // be followed by READDIR, emptying wherever it points.
                    let attrs = self
                        .sftp
                        .symlink_metadata(&path)
                        .await
                        .map_err(|e| map_sftp("lstat", &path, e))?;
                    if entry_kind(&attrs) != EntryKind::Directory {
                        return Err(BlinkError::transport(format!(
                            "{path} is no longer a directory — not deleted",
                        )));
                    }
                    let entries = self
                        .sftp
                        .read_dir(&path)
                        .await
                        .map_err(|e| map_sftp("readdir", &path, e))?;

                    stack.push(Op::Remove(path.clone()));

                    let mut to_recurse: Vec<Op> = Vec::new();
                    for e in entries {
                        let name = e.file_name();
                        if name == "." || name == ".." {
                            continue;
                        }
                        // A name that can't be joined (`..`, `.`) is a
                        // hostile or broken listing entry. Skip it; joining
                        // used to fall back to `path` itself, so this
                        // unlinked the directory being walked.
                        let Some(child) = super::join_remote(&path, &name) else {
                            tracing::warn!(
                                dir = %path,
                                "skipping unusable entry name in recursive delete",
                            );
                            continue;
                        };
                        // Only a real directory is recursed into. A symlink
                        // is a leaf and gets unlinked: recursing through one
                        // would walk outside the subtree the user asked to
                        // delete (and possibly outside the connection's
                        // chroot). Sockets and devices are leaves too.
                        if entry_kind(&e.metadata()) == EntryKind::Directory {
                            to_recurse.push(Op::Visit(child));
                        } else {
                            self.sftp
                                .remove_file(&child)
                                .await
                                .map_err(|err| map_sftp("remove", &child, err))?;
                        }
                    }
                    for op in to_recurse.into_iter().rev() {
                        stack.push(op);
                    }
                }
                Op::Remove(path) => {
                    self.sftp
                        .remove_dir(&path)
                        .await
                        .map_err(|e| map_sftp("rmdir", &path, e))?;
                }
            }
        }
        Ok(())
    }
}

/// Classify an SFTP entry by the whole `S_IFMT` field of its mode.
///
/// Not by `is_dir()` / `is_symlink()` / `is_regular()`: those test whether
/// the type code *contains* a flag's bits, and the codes overlap — a socket
/// or block device contains the directory bit, a symlink the regular-file bit.
/// `file_type()` compares the field for equality. Every site that turns
/// attributes into an [`EntryKind`] goes through here so they cannot drift
/// apart.
fn entry_kind(attrs: &FileAttributes) -> EntryKind {
    match attrs.file_type() {
        FileType::Dir => EntryKind::Directory,
        FileType::Symlink => EntryKind::Symlink,
        FileType::File => EntryKind::File,
        FileType::Other => EntryKind::Other,
    }
}

/// Bytes of a read reply that may be accepted, given how many are still
/// wanted.
///
/// A server must never return more than was asked for. If it does, its idea
/// of the request and ours disagree, and since `pipelined_download` writes
/// chunks in order, accepting the surplus shifts every later chunk and
/// corrupts the file. Refuse instead of guessing which end is wrong.
fn reply_within_request(reply_len: usize, requested: u32, label: &str) -> Result<u32> {
    if reply_len as u64 > u64::from(requested) {
        return Err(BlinkError::transport(format!(
            "{label}: server returned more data than requested \
             ({reply_len} bytes for a {requested}-byte read)"
        )));
    }
    Ok(reply_len as u32)
}

/// Read the byte range `[offset, offset + len)` fully into a `Vec`, looping to
/// absorb short reads (SFTP servers may return fewer bytes than requested).
/// A short reply is followed by a request for the rest; only an empty reply
/// or an `Eof` status marks end-of-file, so the returned `Vec` may be shorter
/// than `len` only at the end of the file. A reply longer than what was asked
/// for is refused — see [`reply_within_request`].
async fn read_full(
    raw: &RawSftpSession,
    handle: &str,
    label: &str,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(len as usize);
    let mut pos = offset;
    let mut remaining = len;
    while remaining > 0 {
        match raw.read(handle, pos, remaining).await {
            Ok(data) => {
                let n = reply_within_request(data.data.len(), remaining, label)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&data.data);
                pos += u64::from(n);
                remaining -= n;
            }
            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
            Err(e) => return Err(map_sftp("read", label, e)),
        }
    }
    Ok(buf)
}

/// Yield `(offset, len)` request specs tiling `[start, end)` in `chunk`-sized
/// pieces. Lazy so a multi-GB file doesn't materialise a giant Vec.
fn chunk_offsets(start: u64, end: u64, chunk: usize) -> impl Iterator<Item = (u64, u32)> {
    let chunk = chunk.max(1) as u64;
    let mut off = start;
    std::iter::from_fn(move || {
        if off >= end {
            return None;
        }
        let len = chunk.min(end - off);
        let spec = (off, len as u32);
        off += len;
        Some(spec)
    })
}

/// Pipelined download: keep `concurrency` range reads in flight at once and
/// write their results to `local` in order. When the server reports a size we
/// tile the range and overlap the requests; otherwise we fall back to a
/// sequential read-until-EOF.
#[allow(clippy::too_many_arguments)]
async fn pipelined_download(
    raw: &Arc<RawSftpSession>,
    handle: &str,
    label: &str,
    start: u64,
    size: Option<u64>,
    chunk: usize,
    concurrency: usize,
    local: &mut super::PartFile,
    total: u64,
    progress: &Option<mpsc::UnboundedSender<ProgressUpdate>>,
) -> Result<u64> {
    let mut done = start;

    if let Some(end) = size {
        let mut stream = futures::stream::iter(chunk_offsets(start, end, chunk))
            .map(|(off, len)| {
                let raw = Arc::clone(raw);
                let handle = handle.to_string();
                let label = label.to_string();
                async move {
                    let data = read_full(&raw, &handle, &label, off, len).await?;
                    Ok::<_, BlinkError>((off, data))
                }
            })
            .buffered(concurrency);

        while let Some(chunk) =
            within_idle("read", label, async { Ok(stream.next().await) }).await?
        {
            let (off, data) = chunk?;
            if data.is_empty() {
                break;
            }
            // Chunks are appended, not written at their offsets, so each must
            // start where the last one ended. One that doesn't follows a
            // chunk that hit end of file early: appending it would put every
            // byte from here on at the wrong place in the file.
            if off != done {
                return Err(BlinkError::transport(format!(
                    "{label}: the server reported end of file at byte {done}, \
                     then sent data from byte {off}"
                )));
            }
            local.write_all(&data).await?;
            done += data.len() as u64;
            if let Some(tx) = progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: done,
                    bytes_total: total,
                });
            }
        }
        drop(stream);
    }

    // Drain anything past the reported size: a file that grew mid-transfer
    // would otherwise be silently truncated, and this is the whole transfer
    // when the size was unknown. Usually a single read that returns EOF.
    loop {
        let data = within_idle(
            "read",
            label,
            read_full(raw, handle, label, done, chunk as u32),
        )
        .await?;
        if data.is_empty() {
            break;
        }
        local.write_all(&data).await?;
        done += data.len() as u64;
        if let Some(tx) = progress {
            let _ = tx.send(ProgressUpdate {
                bytes_done: done,
                bytes_total: total.max(done),
            });
        }
    }

    // The file shrank mid-transfer (a chunk read hit EOF before the size
    // reported at open). Fail rather than rename a short file onto the
    // final path as if the download had succeeded; the stale `.blink-part` is
    // detected and discarded on the next attempt.
    if let Some(end) = size
        && done < end
    {
        return Err(BlinkError::transport(format!(
            "{label}: remote file truncated during transfer \
                 (expected {end} bytes, got {done})"
        )));
    }
    Ok(done)
}

/// Fill `buf` from `src`, looping over short reads until full or EOF. Returns
/// the number of bytes read (`< buf.len()` only at EOF).
async fn read_local_full<R: tokio::io::AsyncRead + Unpin>(
    src: &mut R,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = src.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Move `part` onto `dest`, replacing `dest` if it exists.
///
/// Prefers the `posix-rename@openssh.com` extension, which replaces the
/// target atomically. Without it, SFTP v3 `RENAME` fails on an existing
/// target, so the fallback is: try the plain rename (covers the common
/// no-target case), and when it's refused, move the target aside (see
/// [`super::remote_aside_path`]), rename again, and remove the old file
/// only once that lands; if it doesn't, the old file is moved back. That
/// fallback has a non-atomic window, but it only ever exposes "old file
/// present" or "old file aside, complete new file at `part`" — never a
/// truncated file under the final name, and never neither file.
async fn finalize_remote_rename(
    raw: &RawSftpSession,
    posix_rename: bool,
    part: &str,
    dest: &str,
) -> Result<()> {
    within_idle(
        "rename",
        dest,
        finalize_remote_rename_unbounded(raw, posix_rename, part, dest),
    )
    .await
}

/// [`finalize_remote_rename`] without its deadline.
async fn finalize_remote_rename_unbounded(
    raw: &RawSftpSession,
    posix_rename: bool,
    part: &str,
    dest: &str,
) -> Result<()> {
    if posix_rename {
        // Payload is two SSH strings (u32 BE length + bytes): oldpath, newpath.
        let mut data = Vec::with_capacity(8 + part.len() + dest.len());
        for s in [part, dest] {
            data.extend_from_slice(&(s.len() as u32).to_be_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        use russh_sftp::protocol::Packet;
        return match raw.extended("posix-rename@openssh.com", data).await {
            Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => Ok(()),
            Ok(Packet::Status(s)) => Err(map_sftp("posix-rename", dest, SftpError::Status(s))),
            Ok(_) => Err(BlinkError::transport(format!(
                "posix-rename {dest}: unexpected reply packet"
            ))),
            Err(e) => Err(map_sftp("posix-rename", dest, e)),
        };
    }

    let refused = match raw.rename(part, dest).await {
        Ok(_) => return Ok(()),
        Err(e) => map_sftp("rename", dest, e),
    };
    // Only a file in the way is moved. Nothing there means the rename
    // failed for another reason, and a folder is not this rename's to
    // replace: moved aside, it would stay there, since it can't be removed
    // as a file is.
    let in_the_way = raw
        .lstat(dest)
        .await
        .is_ok_and(|a| entry_kind(&a.attrs) != EntryKind::Directory);
    let aside = super::remote_aside_path(dest);
    if !in_the_way || raw.rename(dest, &aside).await.is_err() {
        return Err(refused);
    }
    match raw.rename(part, dest).await {
        Ok(_) => {
            if let Err(e) = raw.remove(&aside).await {
                tracing::warn!(path = %aside, error = %e, "could not remove the replaced file");
            }
            Ok(())
        }
        Err(e) => {
            let failed = map_sftp("rename", dest, e);
            match raw.rename(&aside, dest).await {
                Ok(_) => Err(failed),
                Err(_) => Err(BlinkError::transport(format!(
                    "{failed}; the file it was replacing is now at {aside}"
                ))),
            }
        }
    }
}

/// Pipelined upload: read the local file in chunks and keep `concurrency`
/// writes in flight. Writes carry explicit offsets, so completion order is
/// irrelevant to correctness; progress reports cumulative acknowledged bytes.
#[allow(clippy::too_many_arguments)]
async fn pipelined_upload(
    raw: &Arc<RawSftpSession>,
    handle: &str,
    label: &str,
    local: &mut tokio::fs::File,
    chunk: usize,
    concurrency: usize,
    total: u64,
    progress: &Option<mpsc::UnboundedSender<ProgressUpdate>>,
) -> Result<()> {
    let chunk = chunk.max(1);
    let mut inflight = FuturesUnordered::new();
    let mut offset = 0u64;
    let mut done = 0u64;
    let mut eof = false;

    while !eof || !inflight.is_empty() {
        while !eof && inflight.len() < concurrency {
            let mut buf = vec![0u8; chunk];
            let n = read_local_full(local, &mut buf).await?;
            if n == 0 {
                eof = true;
                break;
            }
            buf.truncate(n);
            let off = offset;
            offset += n as u64;
            let raw = Arc::clone(raw);
            let handle = handle.to_string();
            let label = label.to_string();
            inflight.push(async move {
                raw.write(handle, off, buf)
                    .await
                    .map(|_| n as u64)
                    .map_err(|e| map_sftp("write", &label, e))
            });
        }

        if let Some(res) = within_idle("write", label, async { Ok(inflight.next().await) }).await? {
            done += res?;
            if let Some(tx) = progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: done,
                    bytes_total: total,
                });
            }
        }
    }
    Ok(())
}

#[async_trait]
impl Transport for SftpTransport {
    fn protocol(&self) -> Protocol {
        Protocol::Sftp
    }

    async fn list(&mut self, remote_path: &str) -> Result<super::Listing> {
        let entries = self
            .sftp
            .read_dir(remote_path)
            .await
            .map_err(|e| map_sftp("readdir", remote_path, e))?;

        let mut out = Vec::new();
        for e in entries {
            // Keep the server's own bytes: `RemoteEntry::new` derives the
            // sanitized form for rendering, and every path blink builds from
            // this entry uses the raw name. Sanitizing here instead would
            // name a file that does not exist.
            let raw_name = e.file_name();
            if raw_name == "." || raw_name == ".." {
                continue;
            }
            let attrs = e.metadata();
            out.push(RemoteEntry::new(
                raw_name,
                entry_kind(&attrs),
                attrs.size.unwrap_or(0),
                attrs
                    .mtime
                    .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0)),
                attrs.permissions,
            ));
        }
        // Structured replies: every entry is read.
        Ok(super::Listing::complete(out))
    }

    async fn download(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()> {
        // Stream into the partial (`part_path`) and rename onto the final
        // path only on success. This keeps the user's existing file (if any) untouched
        // until the download is fully complete and fsynced, and isolates
        // partial bytes from a previous attempt under a recognisable suffix.
        let part = super::part_path(local_path);
        // Before anything looks at the partial: see `transport::lease_part`.
        let lease = super::lease_part(&part).await;
        let origin = self.origin.clone();

        let xfer = self.transfer_session().await?;
        let raw = Arc::clone(&xfer.raw);
        let read_chunk = xfer.read_chunk;

        let remote_handle = within_idle("open", remote_path, async {
            raw.open(remote_path, OpenFlags::READ, FileAttributes::default())
                .await
                .map_err(|e| map_sftp("open", remote_path, e))
        })
        .await?
        .handle;

        // Everything that can fail while the remote file is open runs in
        // here, so the handle is closed on every path out: a local error
        // (resolving the partial, creating the folder, opening the file)
        // used to return early and leave it open on the server.
        let result: Result<(super::PartFile, u64)> = async {
            let attrs = tokio::time::timeout(idle_timeout(), raw.fstat(&remote_handle))
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|a| a.attrs);
            let reported_size = attrs.as_ref().and_then(|a| a.size);
            let stamp = super::RemoteStamp {
                size: reported_size,
                mtime: attrs.as_ref().and_then(|a| a.mtime).map(u64::from),
            };

            let total = reported_size.unwrap_or(0);

            // Resume only a partial we can positively identify as this
            // file — see `transport::decide_resume`. Anything unproven (a
            // partial of a different remote file that shares this local
            // name, one from an older blink, or one whose file has since
            // been replaced) is discarded here so the create below starts
            // from a clean state.
            let offset = super::resume_offset(local_path, remote_path, stamp, &origin).await;

            if let Some(parent) = local_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let mut local = super::open_part(&lease, &part, offset).await?;
            // Record what these bytes are before writing any, so an
            // interrupt at any point leaves an identifiable partial rather
            // than an anonymous one that the next attempt has to throw away.
            super::write_part_meta(local_path, remote_path, stamp, &origin).await;

            let done = pipelined_download(
                &raw,
                &remote_handle,
                remote_path,
                offset,
                reported_size,
                read_chunk,
                TRANSFER_CONCURRENCY,
                &mut local,
                total,
                &progress,
            )
            .await?;
            Ok((local, done))
        }
        .await;
        let _ = tokio::time::timeout(idle_timeout(), raw.close(&remote_handle)).await;
        let (mut local, done) = result?;

        // Flush + fsync the partial so its bytes are durable, then atomically
        // rename onto the final path. Drop the handle first; renaming an
        // open file is fine on Unix but tokio's tempfile/rename pairing is
        // simpler when the source is closed.
        local.flush().await?;
        local.sync_all().await?;
        drop(local);
        super::finish_download(&lease, &part, local_path, done).await?;
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

        let xfer = self.transfer_session().await?;
        let raw = Arc::clone(&xfer.raw);
        let write_chunk = xfer.write_chunk;
        let do_fsync = xfer.fsync;
        let posix_rename = xfer.posix_rename;

        // Stream into the remote partial (`remote_part_path`) and rename
        // onto the final name only on success — the remote mirror of the download path. An
        // interrupted or failed upload must never leave a truncated file
        // under the destination name.
        let part = super::remote_part_path(remote_path);

        let remote_handle = within_idle("open", &part, async {
            raw.open(
                &part,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                FileAttributes::default(),
            )
            .await
            .map_err(|e| map_sftp("open", &part, e))
        })
        .await?
        .handle;

        let result = pipelined_upload(
            &raw,
            &remote_handle,
            remote_path,
            &mut local,
            write_chunk,
            TRANSFER_CONCURRENCY,
            total,
            &progress,
        )
        .await;

        // Durably flush before closing, but only when the server advertised
        // fsync support — otherwise the extended request is just rejected.
        if result.is_ok() && do_fsync {
            let _ = tokio::time::timeout(idle_timeout(), raw.fsync(&remote_handle)).await;
        }
        let _ = tokio::time::timeout(idle_timeout(), raw.close(&remote_handle)).await;

        match result {
            Ok(()) => finalize_remote_rename(&raw, posix_rename, &part, remote_path).await,
            Err(e) => {
                // Uploads don't resume, so a partial has no future value —
                // remove it best-effort so failed uploads don't litter the
                // server. (Pointless on Disconnected, harmless to try.)
                let _ = tokio::time::timeout(idle_timeout(), raw.remove(&part)).await;
                Err(e)
            }
        }
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        // The mapper takes a single `path` for its message; combine
        // from→to so the user can see both endpoints. NotFound /
        // PermissionDenied classification still applies — usually
        // against the source path.
        let label = format!("{from} -> {to}");
        self.sftp
            .rename(from, to)
            .await
            .map_err(|e| map_sftp("rename", &label, e))
    }

    async fn rename_over(&mut self, from: &str, to: &str) -> Result<()> {
        // Without `posix-rename`, the rename below moves `to` aside to make
        // room, and back if it then fails. Asking first spares a source
        // that has gone since the listing that round trip.
        if self.metadata(from).await?.is_none() {
            return Err(BlinkError::not_found(format!("rename {from}")));
        }
        let xfer = self.transfer_session().await?;
        let (raw, posix_rename) = (Arc::clone(&xfer.raw), xfer.posix_rename);
        finalize_remote_rename(&raw, posix_rename, from, to).await
    }

    async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
        self.sftp
            .remove_file(remote_path)
            .await
            .map_err(|e| map_sftp("remove", remote_path, e))
    }

    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
        self.delete_dir_capped(remote_path, recursive, MAX_QUEUED_JOBS)
            .await
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
        self.sftp
            .create_dir(remote_path)
            .await
            .map_err(|e| map_sftp("mkdir", remote_path, e))
    }

    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
        // `Ok(None)` means "this path does not exist on the server" — only
        // a NotFound classification maps there. Everything else (permission
        // denied, connection dropped, unexpected packet) is a real failure
        // that has to propagate; collapsing it to "not found" was masking
        // mid-walk connection drops as "the file disappeared", which was
        // both wrong and confusing in the TUI log.
        let attrs = match self.sftp.metadata(remote_path).await {
            Ok(a) => a,
            Err(e) => match map_sftp("metadata", remote_path, e) {
                BlinkError::NotFound(_) => return Ok(None),
                err => return Err(err),
            },
        };
        let kind = entry_kind(&attrs);
        let raw_name = remote_path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(remote_path)
            .to_string();
        Ok(Some(RemoteEntry::new(
            raw_name,
            kind,
            attrs.size.unwrap_or(0),
            attrs
                .mtime
                .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0)),
            attrs.permissions,
        )))
    }

    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
        let remote = self
            .sftp
            .open_with_flags(remote_path, OpenFlags::READ)
            .await
            .map_err(|e| map_sftp("open", remote_path, e))?;
        let mut buf = Vec::new();
        remote
            .take(MAX_PREVIEW_BYTES + 1)
            .read_to_end(&mut buf)
            .await?;
        if buf.len() as u64 > MAX_PREVIEW_BYTES {
            return Err(BlinkError::transport("file exceeds preview size limit"));
        }
        Ok(Bytes::from(buf))
    }

    async fn close(&mut self) -> Result<()> {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "")
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod host_key_encoding_tests {
    use super::host_key_b64;

    #[test]
    fn a_key_is_encoded_as_known_hosts_stores_it() {
        assert_eq!(
            host_key_b64(Ok::<_, ()>(b"abc".to_vec())).as_deref(),
            Some("YWJj")
        );
    }

    /// A key that would not encode used to become the empty string, which
    /// the user could then be asked to trust, and which every other key
    /// that failed the same way would match.
    #[test]
    fn a_key_that_fails_to_encode_has_no_blob() {
        assert_eq!(host_key_b64(Err::<Vec<u8>, _>("no")), None);
    }

    #[test]
    fn an_empty_encoding_has_no_blob() {
        assert_eq!(host_key_b64(Ok::<_, ()>(Vec::new())), None);
    }
}

#[cfg(test)]
mod key_passphrase_tests {
    use super::super::sftp_test_keys::*;
    use super::*;

    /// Write `key` to a file of its own and load it.
    fn load(key: &str, passphrase: Option<&str>) -> Result<russh::keys::PrivateKey> {
        let home = crate::paths::test_home();
        let path = home.path().join("id");
        std::fs::write(&path, key).unwrap();
        load_key(&path, passphrase)
    }

    const ENCRYPTED: [(&str, &str); 3] = [
        ("OpenSSH", ED25519_KEY_ENCRYPTED),
        ("legacy PEM", RSA_PEM_KEY_ENCRYPTED),
        ("PKCS#8", ECDSA_PKCS8_KEY_ENCRYPTED),
    ];

    #[test]
    fn an_encrypted_key_without_a_passphrase_asks_for_one() {
        for (format, key) in ENCRYPTED {
            assert!(
                matches!(load(key, None), Err(BlinkError::KeyNeedsPassphrase)),
                "{format}"
            );
        }
    }

    #[test]
    fn a_wrong_passphrase_asks_again() {
        for (format, key) in ENCRYPTED {
            assert!(
                matches!(
                    load(key, Some("wrong")),
                    Err(BlinkError::KeyNeedsPassphrase)
                ),
                "{format}"
            );
        }
    }

    #[test]
    fn the_right_passphrase_loads_the_key() {
        for (format, key) in ENCRYPTED {
            assert!(load(key, Some(TEST_PASSPHRASE)).is_ok(), "{format}");
        }
    }

    #[test]
    fn an_unreadable_key_is_a_load_error_not_a_passphrase_prompt() {
        for passphrase in [None, Some("anything")] {
            let err = load("not a key", passphrase).unwrap_err();
            assert!(
                !matches!(err, BlinkError::KeyNeedsPassphrase),
                "{passphrase:?}"
            );
        }
        assert!(load(ED25519_KEY, None).is_ok(), "a plain key needs nothing");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        chunk_offsets, host_key_preference, read_local_full, reply_within_request, rsa_hash_alg,
    };

    #[test]
    fn a_host_seen_before_is_asked_for_its_stored_key_types_first() {
        use super::ssh_key::{Algorithm, EcdsaCurve, HashAlg};

        let default = russh::Preferred::default().key.to_vec();
        assert_eq!(
            host_key_preference(&[]),
            default,
            "a new host: default order"
        );

        let pref = host_key_preference(&["ssh-rsa".to_string()]);
        assert_eq!(
            pref[..3],
            [
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512)
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256)
                },
                Algorithm::Rsa { hash: None },
            ],
            "every RSA signature algorithm verifies a stored ssh-rsa key",
        );
        assert_eq!(
            pref[3],
            Algorithm::Ed25519,
            "the rest follow in default order"
        );
        assert_eq!(pref.len(), default.len(), "nothing is dropped");

        let pref =
            host_key_preference(&["ecdsa-sha2-nistp384".to_string(), "ssh-ed25519".to_string()]);
        assert_eq!(
            pref[..2],
            [
                Algorithm::Ed25519,
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP384
                },
            ],
            "stored types keep the default order among themselves",
        );
    }

    /// OpenSSH 8.8+ disabled `ssh-rsa` (SHA-1) by default in Sept 2021, so an
    /// RSA key that does not ask for `rsa-sha2-512` fails against any current
    /// server. The README lists that as an enforced property; nothing checked
    /// it until now.
    ///
    /// This pins the *request*, which is the part blink controls. What no test
    /// here can pin is the algorithm that ends up on the wire: russh does not
    /// pass the signature algorithm to a server's `auth_publickey` callback,
    /// and `ssh-key`'s verifier accepts a SHA-1 RSA signature, so the
    /// integration test would go green either way.
    #[test]
    fn rsa_keys_request_sha512_and_others_request_nothing() {
        use super::ssh_key::{Algorithm, EcdsaCurve, HashAlg};

        assert_eq!(
            rsa_hash_alg(&Algorithm::Rsa { hash: None }),
            Some(HashAlg::Sha512),
            "an RSA key must ask for rsa-sha2-512",
        );
        assert_eq!(
            rsa_hash_alg(&Algorithm::Rsa {
                hash: Some(HashAlg::Sha256)
            }),
            Some(HashAlg::Sha512),
            "the key's own hash must not lower the requested one",
        );
        assert_eq!(
            rsa_hash_alg(&Algorithm::Ed25519),
            None,
            "Ed25519 has no hash to choose",
        );
        assert_eq!(
            rsa_hash_alg(&Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256
            }),
            None,
            "ECDSA carries its hash in the curve",
        );
    }

    // -- over-long read replies --------------------------------------------
    //
    // `read_full` appended whatever came back and decremented with
    // `saturating_sub`, so a reply longer than the request overran its chunk.
    // `pipelined_download` writes chunks in order, so that shifts every later
    // chunk — a corrupt file from a server we simply believed.

    #[test]
    fn a_reply_within_the_request_is_accepted() {
        assert_eq!(reply_within_request(100, 250, "/f").unwrap(), 100);
        assert_eq!(reply_within_request(250, 250, "/f").unwrap(), 250);
    }

    #[test]
    fn an_over_long_reply_is_refused() {
        let err = reply_within_request(300, 250, "/f").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("more data than requested"), "{msg}");
    }

    #[test]
    fn an_empty_reply_is_accepted_as_eof() {
        assert_eq!(reply_within_request(0, 250, "/f").unwrap(), 0);
    }

    #[test]
    fn chunk_offsets_splits_evenly() {
        let v: Vec<_> = chunk_offsets(0, 1000, 250).collect();
        assert_eq!(v, vec![(0, 250), (250, 250), (500, 250), (750, 250)]);
    }

    #[test]
    fn chunk_offsets_handles_remainder() {
        let v: Vec<_> = chunk_offsets(0, 900, 250).collect();
        assert_eq!(v, vec![(0, 250), (250, 250), (500, 250), (750, 150)]);
    }

    #[test]
    fn chunk_offsets_respects_resume_start() {
        let v: Vec<_> = chunk_offsets(300, 800, 250).collect();
        assert_eq!(v, vec![(300, 250), (550, 250)]);
    }

    #[test]
    fn chunk_offsets_empty_when_start_at_or_past_end() {
        assert!(chunk_offsets(500, 500, 250).next().is_none());
        assert!(chunk_offsets(600, 500, 250).next().is_none());
    }

    // The contiguity invariant: every byte of [0, end) is covered exactly once,
    // with no gaps or overlaps. A violation here corrupts a downloaded file.
    #[test]
    fn chunk_offsets_tile_contiguously() {
        let end = 1_000_003u64;
        let mut expected = 0u64;
        for (off, len) in chunk_offsets(0, end, 261_120) {
            assert_eq!(off, expected, "gap or overlap between chunks");
            assert!(len > 0);
            expected += len as u64;
        }
        assert_eq!(expected, end, "chunks must cover the whole range");
    }

    #[tokio::test]
    async fn read_local_full_fills_then_reports_eof() {
        let mut src = std::io::Cursor::new(vec![7u8; 1000]);
        let mut buf = vec![0u8; 400];

        assert_eq!(read_local_full(&mut src, &mut buf).await.unwrap(), 400);
        assert_eq!(read_local_full(&mut src, &mut buf).await.unwrap(), 400);
        assert_eq!(read_local_full(&mut src, &mut buf).await.unwrap(), 200);
        assert_eq!(read_local_full(&mut src, &mut buf).await.unwrap(), 0);
    }

    // `FileAttributes::is_dir()` and friends are bitwise subset tests, and the
    // `S_IFMT` type codes are not disjoint bit patterns: a socket (0o140000)
    // and a block device (0o060000) both contain the directory bit (0o040000),
    // and a symlink (0o120000) contains the regular-file bit. Classifying with
    // those predicates made sockets directories. Every type code is pinned
    // here, with permission bits set so they cannot be what decides it.
    #[test]
    fn entry_kind_compares_the_whole_file_type_field() {
        use super::entry_kind;
        use crate::transport::EntryKind;
        use russh_sftp::protocol::FileAttributes;

        let cases = [
            (0o010000, EntryKind::Other),     // fifo
            (0o020000, EntryKind::Other),     // character device
            (0o040000, EntryKind::Directory), // directory
            (0o060000, EntryKind::Other),     // block device
            (0o100000, EntryKind::File),      // regular file
            (0o120000, EntryKind::Symlink),   // symlink
            (0o140000, EntryKind::Other),     // socket
        ];
        for (ty, want) in cases {
            let attrs = FileAttributes {
                permissions: Some(ty | 0o7777),
                ..Default::default()
            };
            assert_eq!(entry_kind(&attrs), want, "type code {ty:o}");
        }
    }

    #[test]
    fn entry_kind_without_permissions_is_other() {
        use super::entry_kind;
        use crate::transport::EntryKind;
        use russh_sftp::protocol::FileAttributes;

        assert_eq!(entry_kind(&FileAttributes::default()), EntryKind::Other);
    }
}

/// End-to-end test of the real pipelined [`SftpTransport`] download/upload
/// paths against an in-process russh SSH server fronting a russh-sftp server.
/// No external SSH daemon is required. The server answers reads in short
/// slices to exercise `read_full`'s short-read re-request loop, and the test
/// files are larger than the in-flight window so ordered reassembly is
/// genuinely stressed.
#[cfg(test)]
mod integration {
    use std::collections::HashMap;
    use std::sync::Arc;

    use russh::server::{Auth, ChannelOpenHandle, Msg, Session as ServerSession};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{
        Attrs, Data, FileAttributes, Handle as SftpHandle, OpenFlags, Status, StatusCode, Version,
    };
    use tokio::sync::Mutex;

    use super::{HostKeyDecision, MAX_QUEUED_JOBS, SftpTransport};
    use crate::session::{AuthMethod, Protocol, Session};
    use crate::transport::Transport;
    use crate::tui::event::AppEvent;

    /// Path -> file contents, shared by every SFTP channel on the connection.
    type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    /// Rename the folder `from`, and everything under it, to `to`; false
    /// when there is no folder `from`.
    fn move_folder(store: &mut HashMap<String, Vec<u8>>, from: &str, to: &str) -> bool {
        let prefix = format!("{from}/");
        if !store.contains_key(&prefix) {
            return false;
        }
        let under: Vec<String> = store
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for key in under {
            let data = store.remove(&key).unwrap();
            store.insert(format!("{to}/{}", &key[prefix.len()..]), data);
        }
        true
    }

    /// Cap on bytes returned per read reply. Smaller than the transfer chunk so
    /// each chunk needs several reads, exercising the short-read handling.
    const SHORT_READ: usize = 60_000;

    // ---- SSH server side ------------------------------------------------

    struct SshServer {
        channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
        store: Store,
        /// Refuse every public key, as a server would whose
        /// `authorized_keys` holds none of the client's.
        reject_publickeys: bool,
    }

    // Native `async fn` in trait as of russh 0.60 — see the client Handler.
    impl russh::server::Handler for SshServer {
        type Error = russh::Error;

        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        /// Accept any key. russh verifies the client's signature against the
        /// offered public key *before* calling this, so reaching it at all
        /// means the signature checked out — which is the whole point of the
        /// key-auth tests: they prove the crypto backend verified an RSA or
        /// ECDSA signature, not merely that blink sent something.
        async fn auth_publickey(
            &mut self,
            _user: &str,
            _key: russh::server::VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            if self.reject_publickeys {
                return Ok(Auth::reject());
            }
            Ok(Auth::Accept)
        }

        // russh 0.63 replaced the `Ok(true)`/`Ok(false)` return with an
        // explicit accept/reject handle. Dropping the handle rejects the
        // channel, so `reply.accept()` is what keeps the SFTP subsystem
        // reachable — the old `Ok(true)` has no equivalent in the return type.
        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: ChannelOpenHandle,
            _session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            self.channels.lock().await.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            id: ChannelId,
            name: &str,
            session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            if name == "sftp" {
                let channel = self
                    .channels
                    .lock()
                    .await
                    .remove(&id)
                    .expect("channel was registered on open");
                session.channel_success(id)?;
                let sftp = SftpServer {
                    store: self.store.clone(),
                    listings: std::collections::HashMap::new(),
                };
                // Spawn instead of awaiting: blink opens a second channel for
                // the pipelined transfer session, and awaiting the sftp loop
                // here would block the SSH event loop from ever servicing it.
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), sftp));
            } else {
                session.channel_failure(id)?;
            }
            Ok(())
        }
    }

    // ---- SFTP server side (minimal, in-memory) --------------------------

    struct SftpServer {
        store: Store,
        /// What each open directory handle has left to send. SFTP has no "no
        /// more entries" field: `readdir` is called repeatedly and signals
        /// the end by returning `Eof`, so the server has to remember how far
        /// it got. The listing is taken on the first `readdir` and handed out
        /// [`READDIR_BATCH`] entries at a time.
        listings: std::collections::HashMap<String, std::vec::IntoIter<(String, bool)>>,
    }

    /// Entries per `readdir` reply, as OpenSSH's server sends. One reply for
    /// a whole large directory would be one packet past the 256 KiB russh-sftp
    /// accepts since 3.0.1, which no real server sends.
    const READDIR_BATCH: usize = 100;

    /// POSIX mode bits for the two kinds this store can hold. `FileAttributes`
    /// derives `is_dir` from these, and `is_dir` is what the client's walk
    /// branches on.
    const MODE_DIR: u32 = 0o040_755;
    const MODE_FILE: u32 = 0o100_644;

    /// Immediate children of `dir` as (name, is_dir). A key ending in `/` is
    /// a directory, matching the FTP harness's convention.
    fn children_of(files: &HashMap<String, Vec<u8>>, dir: &str) -> Vec<(String, bool)> {
        let prefix = if dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}/")
        };
        let mut out = Vec::new();
        for path in files.keys() {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let trimmed = rest.trim_end_matches('/');
            if trimmed.is_empty() || trimmed.contains('/') {
                continue;
            }
            out.push((trimmed.to_string(), path.ends_with('/')));
        }
        out
    }

    /// Fault: reads of `path` at an offset in `[start, end)` report end of
    /// file, as a misbehaving server or a file rewritten mid-transfer can.
    /// Stored in the store under [`eof_gap_key`] as `start..end`.
    fn eof_gap_key(path: &str) -> String {
        format!("\0eof-gap:{path}")
    }

    fn in_eof_gap(files: &HashMap<String, Vec<u8>>, path: &str, offset: u64) -> bool {
        let Some(spec) = files.get(&eof_gap_key(path)) else {
            return false;
        };
        let spec = String::from_utf8_lossy(spec);
        let (start, end) = spec.split_once("..").unwrap();
        (start.parse().unwrap()..end.parse().unwrap()).contains(&offset)
    }

    /// Fault: every read and write reply waits this many milliseconds,
    /// stored as decimal text. The server answers one request at a time, so
    /// requests queued behind others wait for all of theirs: a slow link,
    /// as one connection of several sharing it sees. A huge value is a
    /// server that has stopped answering.
    const IO_DELAY_KEY: &str = "\0io-delay-ms";

    async fn io_delay(store: &Store) {
        let ms = store
            .lock()
            .await
            .get(IO_DELAY_KEY)
            .map(|v| String::from_utf8_lossy(v).parse::<u64>().unwrap());
        if let Some(ms) = ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
    }

    /// Marks `path` as a symlink. The path's own entries (a `path/`
    /// directory and what's under it) are what following the link finds;
    /// only `lstat` sees the mark.
    fn symlink_key(path: &str) -> String {
        format!("\0symlink:{path}")
    }

    /// Present while a file handle on `path` is open: set by `open`, cleared
    /// by `close`, so a test can see a handle left open.
    fn open_handle_key(path: &str) -> String {
        format!("\0open:{path}")
    }

    fn ok_status(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        }
    }

    impl russh_sftp::server::Handler for SftpServer {
        type Error = StatusCode;

        fn unimplemented(&self) -> StatusCode {
            StatusCode::OpUnsupported
        }

        async fn init(
            &mut self,
            _version: u32,
            _extensions: HashMap<String, String>,
        ) -> Result<Version, Self::Error> {
            Ok(Version::new())
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            pflags: OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<SftpHandle, Self::Error> {
            let mut store = self.store.lock().await;
            if pflags.contains(OpenFlags::WRITE) {
                // CREATE | TRUNCATE semantics for the upload path.
                store.insert(filename.clone(), Vec::new());
            } else if !store.contains_key(&filename) {
                return Err(StatusCode::NoSuchFile);
            }
            store.insert(open_handle_key(&filename), Vec::new());
            Ok(SftpHandle {
                id,
                handle: filename,
            })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.store.lock().await.remove(&open_handle_key(&handle));
            Ok(ok_status(id))
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            io_delay(&self.store).await;
            let store = self.store.lock().await;
            let content = store.get(&handle).ok_or(StatusCode::NoSuchFile)?;
            let off = offset as usize;
            if off >= content.len() || in_eof_gap(&store, &handle, offset) {
                return Err(StatusCode::Eof);
            }
            // Deliberately short: forces the client to re-request the rest.
            let want = (len as usize).min(SHORT_READ);
            let end = (off + want).min(content.len());
            Ok(Data {
                id,
                data: content[off..end].to_vec(),
            })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            io_delay(&self.store).await;
            let mut store = self.store.lock().await;
            let content = store.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
            let off = offset as usize;
            let end = off + data.len();
            if content.len() < end {
                content.resize(end, 0);
            }
            content[off..end].copy_from_slice(&data);
            Ok(ok_status(id))
        }

        async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
            let store = self.store.lock().await;
            let size = store.get(&handle).map(|c| c.len() as u64).unwrap_or(0);
            // A directory is stored under a key ending in `/`, but the client
            // asks about it without one.
            let is_dir = store.contains_key(&format!("{handle}/"));
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(size),
                    permissions: Some(if is_dir { MODE_DIR } else { MODE_FILE }),
                    atime: Some(HARNESS_MTIME),
                    mtime: Some(HARNESS_MTIME),
                    ..Default::default()
                },
            })
        }

        async fn mkdir(
            &mut self,
            id: u32,
            path: String,
            _attrs: FileAttributes,
        ) -> Result<Status, Self::Error> {
            // A directory is a key ending in `/`. As a real server does,
            // refuse a name already taken, by a directory or by a file.
            let path = path.trim_end_matches('/').to_string();
            let mut store = self.store.lock().await;
            if store.contains_key(&path) || store.contains_key(&format!("{path}/")) {
                return Err(StatusCode::Failure);
            }
            store.insert(format!("{path}/"), Vec::new());
            Ok(ok_status(id))
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<SftpHandle, Self::Error> {
            let key = format!("{}/", path.trim_end_matches('/'));
            if !self.store.lock().await.contains_key(&key) {
                return Err(StatusCode::NoSuchFile);
            }
            // Namespaced so a directory handle cannot be mistaken for the
            // file handle of the same path, which `fstat` would size.
            let handle = format!("dir:{key}");
            self.listings.remove(&handle);
            Ok(SftpHandle { id, handle })
        }

        async fn readdir(
            &mut self,
            id: u32,
            handle: String,
        ) -> Result<russh_sftp::protocol::Name, Self::Error> {
            if !self.listings.contains_key(&handle) {
                let Some(dir) = handle.strip_prefix("dir:") else {
                    return Err(StatusCode::Failure);
                };
                let children = children_of(&*self.store.lock().await, dir);
                self.listings.insert(handle.clone(), children.into_iter());
            }
            let remaining = self.listings.get_mut(&handle).expect("inserted above");
            let batch: Vec<_> = remaining.by_ref().take(READDIR_BATCH).collect();
            if batch.is_empty() {
                return Err(StatusCode::Eof);
            }
            let files = batch
                .into_iter()
                .map(|(name, is_dir)| {
                    russh_sftp::protocol::File::new(
                        name,
                        FileAttributes {
                            size: Some(0),
                            permissions: Some(if is_dir { MODE_DIR } else { MODE_FILE }),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            Ok(russh_sftp::protocol::Name { id, files })
        }

        async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
            let key = format!("{}/", path.trim_end_matches('/'));
            let mut store = self.store.lock().await;
            if !store.contains_key(&key) {
                return Err(StatusCode::NoSuchFile);
            }
            // Real servers refuse to unlink a directory that still holds
            // entries; the walk's bottom-up order is only a contract if the
            // server would punish getting it wrong.
            if !children_of(&store, &key).is_empty() {
                return Err(StatusCode::Failure);
            }
            store.remove(&key);
            Ok(ok_status(id))
        }

        /// `stat` without following a symlink: a path marked with
        /// [`symlink_key`] is a link, whatever `stat` makes of it.
        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            if self.store.lock().await.contains_key(&symlink_key(&path)) {
                return Ok(Attrs {
                    id,
                    attrs: FileAttributes {
                        size: Some(0),
                        permissions: Some(0o120_777),
                        atime: Some(HARNESS_MTIME),
                        mtime: Some(HARNESS_MTIME),
                        ..Default::default()
                    },
                });
            }
            self.stat(id, path).await
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            // A path that is in the store neither as a file nor as a
            // directory does not exist, as a real server would say.
            {
                let store = self.store.lock().await;
                if !store.contains_key(&path) && !store.contains_key(&format!("{path}/")) {
                    return Err(StatusCode::NoSuchFile);
                }
            }
            self.fstat(id, path).await
        }

        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<Status, Self::Error> {
            let mut store = self.store.lock().await;
            // SFTP v3 semantics: RENAME fails when the target exists, file
            // or folder. This deliberately exercises the client's
            // move-aside-then-rename fallback for upload finalization.
            if store.contains_key(&newpath) || store.contains_key(&format!("{newpath}/")) {
                return Err(StatusCode::Failure);
            }
            if move_folder(&mut store, &oldpath, &newpath) {
                return Ok(ok_status(id));
            }
            match store.remove(&oldpath) {
                Some(data) => {
                    store.insert(newpath, data);
                    Ok(ok_status(id))
                }
                None => Err(StatusCode::NoSuchFile),
            }
        }

        async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
            let mut store = self.store.lock().await;
            if store.remove(&filename).is_some() {
                Ok(ok_status(id))
            } else {
                Err(StatusCode::NoSuchFile)
            }
        }
    }

    /// Deterministic pseudo-random bytes (xorshift64) so the test is
    /// reproducible without pulling in an RNG dependency.
    fn pseudo_random(n: usize) -> Vec<u8> {
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s & 0xff) as u8
            })
            .collect()
    }

    /// Start the in-process SSH+SFTP server. Returns the bound port and a
    /// counter of accepted TCP connections (used to assert that the
    /// dispatcher's connection pool actually reuses connections).
    async fn start_server(store: Store) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        start_server_with_host_key(store, super::super::sftp_test_keys::ED25519_KEY).await
    }

    /// As [`start_server`], but with the host key given as an OpenSSH-format
    /// private key. The algorithm the server proves its identity with is what
    /// `check_server_key` verifies and what `known_hosts` records a keytype
    /// for, so it has to be variable to be tested.
    async fn start_server_with_host_key(
        store: Store,
        host_key: &str,
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        start_server_with_host_keys(store, &[host_key]).await
    }

    /// As [`start_server_with_host_key`], for a server holding several host
    /// keys — the case where the client's preference picks which one it
    /// proves itself with.
    async fn start_server_with_host_keys(
        store: Store,
        host_keys: &[&str],
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        start_server_inner(store, host_keys, false).await
    }

    /// As [`start_server`], but refusing every public key.
    async fn start_server_rejecting_keys(store: Store) -> u16 {
        let keys = [super::super::sftp_test_keys::ED25519_KEY];
        start_server_inner(store, &keys, true).await.0
    }

    async fn start_server_inner(
        store: Store,
        host_keys: &[&str],
        reject_publickeys: bool,
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(russh::server::Config {
            keys: host_keys
                .iter()
                .map(|k| {
                    russh::keys::PrivateKey::from_openssh(k).expect("test host key should parse")
                })
                .collect(),
            ..Default::default()
        });

        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connects_l = Arc::clone(&connects);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                connects_l.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let cfg = config.clone();
                let handler = SshServer {
                    channels: Arc::new(Mutex::new(HashMap::new())),
                    store: store.clone(),
                    reject_publickeys,
                };
                tokio::spawn(async move {
                    // RunningSession is dropped here; the session task it spawns
                    // keeps running (a dropped JoinHandle detaches, not aborts).
                    let _ = russh::server::run_stream(cfg, socket, handler).await;
                });
            }
        });
        (port, connects)
    }

    /// "Trust once" must cover every connection the session opens, not just
    /// the first. Each dispatcher worker opens its own; before the shared
    /// trust store, every one of them raised a fresh host-key prompt
    /// mid-transfer — and that prompt takes over the whole UI.
    /// End to end: a prompt answered after the connect deadline would have
    /// passed still connects, because time with the prompt open does not
    /// count. The deadline is shortened to half a second and the answer
    /// comes after a full one.
    #[tokio::test]
    async fn a_prompt_answered_after_the_connect_deadline_still_connects() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c) = start_server(store).await;
        let session = test_session(port);
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let (user_wait, waiting) = crate::transport::UserWait::new();

        let connect = crate::transport::within_deadline_excluding_waits(
            std::time::Duration::from_millis(500),
            waiting,
            SftpTransport::connect(
                &session,
                Some("pw"),
                ev_tx,
                crate::known_hosts::SessionTrust::new(),
                user_wait,
            ),
        );
        let answer_slowly = async {
            if let Some(AppEvent::HostKeyUnknown { decision_tx, .. }) = ev_rx.recv().await {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let _ = decision_tx.send(HostKeyDecision::AcceptOnce);
            }
        };
        let (result, ()) = tokio::join!(connect, answer_slowly);
        let mut transport = result
            .expect("the deadline must not run while the prompt is open")
            .expect("and the connect must succeed");
        let _ = transport.close().await;
    }

    #[tokio::test]
    async fn accept_once_covers_later_connections_of_the_same_session() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c) = start_server(store).await;
        let session = test_session(port);
        let trust = crate::known_hosts::SessionTrust::new();

        // First connection: unknown key, user picks "trust once".
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let mut first = Box::pin(SftpTransport::connect(
            &session,
            Some("pw"),
            ev_tx,
            trust.clone(),
            crate::transport::UserWait::new().0,
        ));
        let mut prompts = 0usize;
        let mut first = loop {
            tokio::select! {
                res = &mut first => break res.expect("first connect"),
                Some(ev) = ev_rx.recv() => {
                    if let AppEvent::HostKeyUnknown { decision_tx, .. } = ev {
                        prompts += 1;
                        let _ = decision_tx.send(HostKeyDecision::AcceptOnce);
                    }
                }
            }
        };
        assert_eq!(prompts, 1, "the first connection must ask");

        // Second connection, same session, same trust store — as a worker
        // would. It must not ask again.
        let (ev_tx2, mut ev_rx2) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let mut second = SftpTransport::connect(
            &session,
            Some("pw"),
            ev_tx2,
            trust,
            crate::transport::UserWait::new().0,
        )
        .await
        .expect("second connect should succeed without a prompt");

        assert!(
            ev_rx2.try_recv().is_err(),
            "a session-trusted key must not raise a second prompt",
        );

        let _ = first.close().await;
        let _ = second.close().await;
    }

    /// The modification time the harness reports for every stored file.
    const HARNESS_MTIME: u32 = 1_767_268_800;

    /// The sidecar an interrupted download of a `len`-byte harness file
    /// leaves.
    fn harness_stamp(len: usize) -> crate::transport::RemoteStamp {
        crate::transport::RemoteStamp {
            size: Some(len as u64),
            mtime: Some(u64::from(HARNESS_MTIME)),
        }
    }

    fn test_session(port: u16) -> Session {
        Session {
            name: "it".to_string(),
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        }
    }

    /// A nested tree under `/tree`, plus a sibling the walk must not touch.
    /// Mirrors the FTP harness's `tree_store`.
    async fn tree_store() -> Store {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/tree/".to_string(), Vec::new());
            files.insert("/tree/a.txt".to_string(), b"a".to_vec());
            files.insert("/tree/sub/".to_string(), Vec::new());
            files.insert("/tree/sub/b.txt".to_string(), b"b".to_vec());
            files.insert("/sibling.txt".to_string(), b"keep".to_vec());
        }
        store
    }

    /// `n` sibling directories under `/wide`. Only directories accumulate on
    /// the delete walk's stack — files are unlinked inline — so width is what
    /// drives it towards the ceiling.
    async fn wide_store(n: usize) -> Store {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/wide/".to_string(), Vec::new());
            for i in 0..n {
                files.insert(format!("/wide/d{i}/"), Vec::new());
            }
        }
        store
    }

    /// The recursive SFTP delete walk, which had no coverage at all until the
    /// harness learned to list directories. The server refuses `rmdir` on a
    /// directory that still holds entries, so bottom-up order is not merely
    /// asserted here — getting it wrong fails the delete outright.
    #[tokio::test]
    async fn a_recursive_delete_removes_a_nested_tree_bottom_up() {
        let store = tree_store().await;
        let (port, _connects) = start_server(store.clone()).await;
        let mut transport = connect(port).await;

        transport
            .delete_dir("/tree", true)
            .await
            .expect("recursive delete should succeed");

        let mut remaining: Vec<String> = store.lock().await.keys().cloned().collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec!["/sibling.txt".to_string()],
            "the walk should take the tree and nothing else",
        );
    }

    /// The guard itself: a listing wider than the ceiling must stop the walk
    /// rather than let the stack grow with it. Run at a small limit so the
    /// behaviour is visible without a tree of `MAX_QUEUED_JOBS` entries;
    /// `a_recursive_delete_refuses_a_tree_wider_than_the_real_cap` pins that
    /// the production entry point supplies the real one.
    #[tokio::test]
    async fn a_recursive_delete_does_not_follow_a_directory_swapped_for_a_symlink() {
        let store = tree_store().await;
        store
            .lock()
            .await
            .insert(symlink_key("/tree/sub"), Vec::new());
        let (port, _) = start_server(Arc::clone(&store)).await;
        let mut t = connect(port).await;

        let err = t.delete_dir("/tree", true).await.unwrap_err();
        assert!(err.to_string().contains("no longer a directory"), "{err}");
        let files = store.lock().await;
        assert!(
            files.contains_key("/tree/sub/b.txt"),
            "nothing behind the link is deleted"
        );
    }

    #[tokio::test]
    async fn a_recursive_delete_stops_once_pending_work_passes_the_cap() {
        let store = wide_store(4).await;
        let (port, _connects) = start_server(store.clone()).await;
        let mut transport = connect(port).await;

        let err = transport
            .delete_dir_capped("/wide", true, 2)
            .await
            .expect_err("a listing wider than the ceiling must be refused");

        assert!(
            err.to_string().contains("exceeded") && err.to_string().contains('2'),
            "expected the delete budget error naming the limit, got: {err}",
        );

        // The guard has to fire before the walk acts, not after: a partially
        // deleted tree would be worse than a refused one.
        assert_eq!(
            store.lock().await.len(),
            5,
            "nothing should have been removed",
        );
    }

    /// The production entry point must supply the *real* ceiling, not merely
    /// have one. Cheap despite its size: with the guard in place the walk
    /// stops on the iteration after the first listing, so this reads one
    /// directory and removes nothing.
    #[tokio::test]
    async fn a_recursive_delete_refuses_a_tree_wider_than_the_real_cap() {
        let store = wide_store(MAX_QUEUED_JOBS + 1).await;
        let (port, _connects) = start_server(store.clone()).await;
        let mut transport = connect(port).await;

        let err = transport
            .delete_dir("/wide", true)
            .await
            .expect_err("a tree this wide must not be walked");

        assert!(
            err.to_string().contains(&MAX_QUEUED_JOBS.to_string()),
            "the error must name the real ceiling, got: {err}",
        );
        assert_eq!(
            store.lock().await.len(),
            MAX_QUEUED_JOBS + 2,
            "nothing should have been removed",
        );
    }

    async fn connect(port: u16) -> SftpTransport {
        let session = Session {
            name: "it".to_string(),
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        };
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let mut fut = Box::pin(SftpTransport::connect(
            &session,
            Some("pw"),
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
            crate::transport::UserWait::new().0,
        ));
        loop {
            tokio::select! {
                res = &mut fut => return res.expect("connect should succeed"),
                Some(ev) = ev_rx.recv() => {
                    // First connect to this ephemeral host is always an unknown
                    // key; trust it for the session so the handshake proceeds.
                    if let AppEvent::HostKeyUnknown { decision_tx, .. } = ev {
                        let _ = decision_tx.send(HostKeyDecision::AcceptOnce);
                    }
                }
            }
        }
    }

    // --- scp:// ---
    //
    // SCP is SFTP under another name: every method of `ScpTransport` hands
    // straight to an `SftpTransport`. These run each one against the server
    // to show the hand-off reaches it.

    async fn connect_scp(port: u16) -> crate::transport::scp::ScpTransport {
        let mut session = test_session(port);
        session.protocol = Protocol::Scp;
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let mut fut = Box::pin(crate::transport::scp::ScpTransport::connect(
            &session,
            Some("pw"),
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
            crate::transport::UserWait::new().0,
        ));
        loop {
            tokio::select! {
                res = &mut fut => return res.expect("connect should succeed"),
                Some(ev) = ev_rx.recv() => {
                    if let AppEvent::HostKeyUnknown { decision_tx, .. } = ev {
                        let _ = decision_tx.send(HostKeyDecision::AcceptOnce);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn scp_reports_itself_as_scp_and_does_everything_through_sftp() {
        let home = crate::paths::test_home();
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _connects) = start_server(store.clone()).await;
        let mut t = connect_scp(port).await;
        assert_eq!(t.protocol(), Protocol::Scp);

        t.mkdir("/d").await.unwrap();
        assert!(store.lock().await.contains_key("/d/"));

        let up = home.path().join("up.txt");
        std::fs::write(&up, b"hello").unwrap();
        t.upload(&up, "/d/a.txt", None).await.unwrap();
        assert_eq!(
            store.lock().await.get("/d/a.txt").map(Vec::as_slice),
            Some(&b"hello"[..])
        );

        let listing = t.list("/d").await.unwrap();
        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|e| e.raw_name.as_str())
            .collect();
        assert!(names.contains(&"a.txt"), "{names:?}");

        let meta = t.metadata("/d/a.txt").await.unwrap().expect("exists");
        assert_eq!(meta.size, 5);
        assert!(t.metadata("/d/none").await.unwrap().is_none());

        assert_eq!(&t.read_to_bytes("/d/a.txt").await.unwrap()[..], b"hello");

        let down = home.path().join("down.txt");
        t.download("/d/a.txt", &down, None).await.unwrap();
        assert_eq!(std::fs::read(&down).unwrap(), b"hello");

        t.rename("/d/a.txt", "/d/b.txt").await.unwrap();
        store
            .lock()
            .await
            .insert("/d/c.txt".to_string(), b"old".to_vec());
        t.rename_over("/d/b.txt", "/d/c.txt").await.unwrap();
        assert_eq!(
            store.lock().await.get("/d/c.txt").map(Vec::as_slice),
            Some(&b"hello"[..])
        );

        t.delete_file("/d/c.txt").await.unwrap();
        t.delete_dir("/d", false).await.unwrap();
        assert!(
            store.lock().await.is_empty(),
            "{:?}",
            store.lock().await.keys()
        );

        t.close().await.unwrap();
    }

    /// Connect with `auth`, returning the transport and the host key type the
    /// TOFU prompt reported. The key type is the only place the negotiated
    /// host-key algorithm is observable from outside — `SessionTrust` keeps
    /// the decision in memory, so nothing reaches a `known_hosts` file to
    /// read back, and test isolation would forbid touching the real one.
    async fn connect_with(port: u16, auth: AuthMethod) -> (SftpTransport, String) {
        let session = Session {
            name: "it".to_string(),
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        };
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let mut fut = Box::pin(SftpTransport::connect(
            &session,
            Some("pw"),
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
            crate::transport::UserWait::new().0,
        ));
        let mut seen_key_type = String::new();
        loop {
            tokio::select! {
                res = &mut fut => {
                    return (res.expect("connect should succeed"), seen_key_type);
                }
                Some(ev) = ev_rx.recv() => {
                    if let AppEvent::HostKeyUnknown { decision_tx, key_type, .. } = ev {
                        seen_key_type = key_type;
                        let _ = decision_tx.send(HostKeyDecision::AcceptOnce);
                    }
                }
            }
        }
    }

    /// Connect to `port` without answering any host-key prompt, reporting
    /// whether one was raised and whether the key was refused as changed.
    async fn connect_unprompted(port: u16) -> (crate::error::Result<SftpTransport>, bool, bool) {
        connect_unprompted_trusting(port, crate::known_hosts::SessionTrust::new()).await
    }

    /// As [`connect_unprompted`], with `trust` as the session's accept-once
    /// store.
    async fn connect_unprompted_trusting(
        port: u16,
        trust: crate::known_hosts::SessionTrust,
    ) -> (crate::error::Result<SftpTransport>, bool, bool) {
        let session = Session {
            name: "it".to_string(),
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        };
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let mut fut = Box::pin(SftpTransport::connect(
            &session,
            Some("pw"),
            ev_tx,
            trust,
            crate::transport::UserWait::new().0,
        ));
        let (mut prompted, mut changed) = (false, false);
        loop {
            tokio::select! {
                res = &mut fut => {
                    // The handler sends its event before the connect fails,
                    // but `select!` may see both ready and take this arm.
                    while let Ok(ev) = ev_rx.try_recv() {
                        prompted |= matches!(ev, AppEvent::HostKeyUnknown { .. });
                        changed |= matches!(ev, AppEvent::HostKeyChanged { .. });
                    }
                    return (res, prompted, changed);
                }
                Some(ev) = ev_rx.recv() => match ev {
                    AppEvent::HostKeyUnknown { decision_tx, .. } => {
                        prompted = true;
                        let _ = decision_tx.send(HostKeyDecision::Reject);
                    }
                    AppEvent::HostKeyChanged { .. } => changed = true,
                    _ => {}
                },
            }
        }
    }

    /// `pem`'s public key as its type and base64 blob, as known_hosts holds it.
    fn host_key_entry(pem: &str) -> (String, String) {
        use base64::Engine as _;
        let public = russh::keys::PrivateKey::from_openssh(pem)
            .unwrap()
            .public_key()
            .clone();
        let b64 = base64::engine::general_purpose::STANDARD.encode(public.to_bytes().unwrap());
        (public.algorithm().as_str().to_string(), b64)
    }

    /// Store `pem`'s public key in this test's known_hosts for 127.0.0.1.
    fn store_host_key(port: u16, pem: &str) {
        let (key_type, b64) = host_key_entry(pem);
        crate::known_hosts::append("127.0.0.1", port, &key_type, &b64).unwrap();
    }

    /// A key trusted once vouches for its host for the session. A worker
    /// connection that meets another key there is refused as a changed key —
    /// not prompted for, where a stray keypress could accept the impostor.
    #[tokio::test]
    async fn a_key_other_than_the_one_trusted_once_is_refused() {
        let _home = crate::paths::test_home();
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c) =
            start_server_with_host_key(store, super::super::sftp_test_keys::ED25519_KEY).await;
        let trust = crate::known_hosts::SessionTrust::new();
        let (key_type, b64) = host_key_entry(super::super::sftp_test_keys::RSA_KEY);
        trust.trust("127.0.0.1", port, &key_type, &b64);

        let (res, prompted, changed) = connect_unprompted_trusting(port, trust).await;
        assert!(res.is_err(), "the connection must be refused");
        assert!(changed, "as a changed key");
        assert!(!prompted, "never as a first connection");
    }

    #[tokio::test]
    async fn the_key_trusted_once_is_accepted_again() {
        let _home = crate::paths::test_home();
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c) =
            start_server_with_host_key(store, super::super::sftp_test_keys::ED25519_KEY).await;
        let trust = crate::known_hosts::SessionTrust::new();
        let (key_type, b64) = host_key_entry(super::super::sftp_test_keys::ED25519_KEY);
        trust.trust("127.0.0.1", port, &key_type, &b64);

        let (res, prompted, changed) = connect_unprompted_trusting(port, trust).await;
        assert!(!prompted && !changed);
        res.expect("the trusted key is accepted")
            .close()
            .await
            .unwrap();
    }

    /// A server with several host keys proves itself with the one the client
    /// asks for first. blink asks for the stored type first, so a known host
    /// that also holds a key of a type blink prefers is still recognised —
    /// not prompted for as if new, and not refused as changed.
    #[tokio::test]
    async fn a_known_host_is_recognised_by_its_stored_key_type() {
        let _home = crate::paths::test_home();
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c) = start_server_with_host_keys(
            store,
            &[
                super::super::sftp_test_keys::ED25519_KEY,
                super::super::sftp_test_keys::RSA_KEY,
            ],
        )
        .await;
        store_host_key(port, super::super::sftp_test_keys::RSA_KEY);

        let (res, prompted, changed) = connect_unprompted(port).await;
        assert!(!prompted, "a known host must not be prompted for");
        assert!(!changed, "nor refused as changed");
        res.expect("the stored RSA key must be negotiated and trusted")
            .close()
            .await
            .unwrap();
    }

    /// The downgrade: a known host that presents only a key of a type not
    /// stored for it is refused as changed, not offered as a new host.
    #[tokio::test]
    async fn a_known_host_presenting_a_new_key_type_is_refused() {
        let _home = crate::paths::test_home();
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c) =
            start_server_with_host_key(store, super::super::sftp_test_keys::ED25519_KEY).await;
        store_host_key(port, super::super::sftp_test_keys::RSA_KEY);

        let (res, prompted, changed) = connect_unprompted(port).await;
        assert!(res.is_err(), "the connection must be refused");
        assert!(changed, "as a changed key");
        assert!(!prompted, "never as a first connection");
    }

    /// Write a fixture key where `AuthMethod::Key` can find it. Named per
    /// test so concurrent tests cannot race on one path, as the FTP harness's
    /// `tempdir_for_test` does.
    fn write_key_fixture(tag: &str, pem: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blink-sftp-key-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("id_test");
        std::fs::write(&path, pem).unwrap();
        path
    }

    /// The host key is what `check_server_key` verifies, so its algorithm is
    /// the crypto path a backend swap actually exercises. Before this, the
    /// harness only ever offered Ed25519 — the one algorithm whose signatures
    /// say nothing about whether RSA or ECDSA verification works.
    #[tokio::test]
    async fn a_server_host_key_is_verified_for_every_algorithm() {
        for (label, pem, expected_type) in [
            (
                "ed25519",
                super::super::sftp_test_keys::ED25519_KEY,
                "ssh-ed25519",
            ),
            (
                "ecdsa",
                super::super::sftp_test_keys::ECDSA_KEY,
                "ecdsa-sha2-nistp256",
            ),
            ("rsa", super::super::sftp_test_keys::RSA_KEY, "ssh-rsa"),
        ] {
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let (port, _c) = start_server_with_host_key(store, pem).await;

            let (mut transport, key_type) = connect_with(port, AuthMethod::Password).await;

            assert_eq!(
                key_type, expected_type,
                "{label}: the prompt should name the host key's own algorithm",
            );
            transport.close().await.expect("close should be clean");
        }
    }

    /// `AuthMethod::Key` had no coverage against a server at all: every
    /// existing test authenticates with a password, so the whole key-auth
    /// path — including the RSA hash negotiation below — was never run.
    ///
    /// The server accepts any key, but russh verifies the signature before
    /// the accept callback runs, so a pass here means the backend really did
    /// verify an ECDSA or RSA signature.
    #[tokio::test]
    async fn client_key_authentication_works_for_every_algorithm() {
        for (tag, pem) in [
            ("ed25519", super::super::sftp_test_keys::ED25519_KEY),
            ("ecdsa", super::super::sftp_test_keys::ECDSA_KEY),
            ("rsa", super::super::sftp_test_keys::RSA_KEY),
        ] {
            let store: Store = Arc::new(Mutex::new(HashMap::new()));
            let (port, _c) = start_server(store).await;
            let path = write_key_fixture(tag, pem);

            let (mut transport, _) =
                connect_with(port, AuthMethod::Key { path: path.clone() }).await;

            transport
                .close()
                .await
                .unwrap_or_else(|e| panic!("{tag}: close should be clean: {e}"));
            let _ = std::fs::remove_file(&path);
        }
    }

    #[tokio::test]
    async fn a_download_that_fails_locally_still_closes_the_remote_file() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/f.bin".to_string(), b"data".to_vec());
        let (port, _connects) = start_server(store.clone()).await;
        let mut transport = connect(port).await;
        // The local parent is a file, so creating it fails after the
        // remote file is open.
        let home = crate::paths::test_home();
        let blocker = home.path().join("blocker");
        std::fs::write(&blocker, b"").unwrap();

        assert!(
            transport
                .download("/f.bin", &blocker.join("f.bin"), None)
                .await
                .is_err()
        );
        assert!(
            !store.lock().await.contains_key(&open_handle_key("/f.bin")),
            "the remote handle must be closed"
        );
    }

    /// A download on a slow connection completes when its reads wait in
    /// line longer than the deadline, as long as one of them finishes inside
    /// it.
    ///
    /// The pipeline keeps 16 reads queued, so the last one waits for the 15
    /// ahead of it. With several downloads sharing a link, that wait passed
    /// russh-sftp's 10 s per-request limit, which starts when a request is
    /// sent. The read then failed as a lost connection, and the job failed
    /// with its partial left on disk. Here each reply takes 100 ms, so the
    /// last read of the first round waits about 1.6 s against a 1 s deadline.
    #[tokio::test]
    async fn a_slow_download_whose_reads_queue_past_the_deadline_completes() {
        super::IDLE_TIMEOUT.set(std::time::Duration::from_secs(1));
        let payload = pseudo_random(16 * SHORT_READ);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/slow.bin".to_string(), payload.clone());
            files.insert(IO_DELAY_KEY.to_string(), b"100".to_vec());
        }
        let (port, _connects) = start_server(store).await;
        let mut transport = connect(port).await;
        let raw = Arc::clone(&transport.transfer_session().await.unwrap().raw);
        let handle = raw
            .open("/slow.bin", OpenFlags::READ, FileAttributes::default())
            .await
            .unwrap()
            .handle;
        let home = crate::paths::test_home();
        let path = home.path().join("slow.bin");
        let lease = crate::transport::lease_part(&path).await;
        let mut local = crate::transport::open_part(&lease, &path, 0).await.unwrap();

        // One reply per read, so the queue is the 16 reads and nothing else.
        with_timeout(
            super::pipelined_download(
                &raw,
                &handle,
                "/slow.bin",
                0,
                Some(payload.len() as u64),
                SHORT_READ,
                super::TRANSFER_CONCURRENCY,
                &mut local,
                payload.len() as u64,
                &None,
            ),
            "slow download",
        )
        .await
        .expect("reads waiting their turn are not a stalled connection");
        drop(local);
        assert!(std::fs::read(&path).unwrap() == payload, "bytes mismatch");
    }

    /// A server that stops answering fails the download after the idle
    /// deadline, and fails it as a lost connection, so the dispatcher
    /// closes it rather than pooling it.
    #[tokio::test]
    async fn a_download_from_a_server_that_stops_answering_fails() {
        super::IDLE_TIMEOUT.set(std::time::Duration::from_millis(500));
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/f.bin".to_string(), pseudo_random(SHORT_READ));
            files.insert(IO_DELAY_KEY.to_string(), b"3600000".to_vec());
        }
        let (port, _connects) = start_server(store).await;
        let mut transport = connect(port).await;
        let home = crate::paths::test_home();

        // Well under russh-sftp's own 10 s limit, which is what ended a
        // stall before.
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transport.download("/f.bin", &home.path().join("f.bin"), None),
        )
        .await
        .expect("a stalled download must fail after the idle deadline")
        .expect_err("and must not succeed");
        assert!(
            matches!(err, crate::error::BlinkError::Disconnected(_)),
            "{err:?}"
        );
    }

    /// As above, for an upload.
    #[tokio::test]
    async fn an_upload_to_a_server_that_stops_answering_fails() {
        super::IDLE_TIMEOUT.set(std::time::Duration::from_millis(500));
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert(IO_DELAY_KEY.to_string(), b"3600000".to_vec());
        let (port, _connects) = start_server(store).await;
        let mut transport = connect(port).await;
        let home = crate::paths::test_home();
        let src = home.path().join("up.bin");
        std::fs::write(&src, pseudo_random(SHORT_READ)).unwrap();

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transport.upload(&src, "/up.bin", None),
        )
        .await
        .expect("a stalled upload must fail after the idle deadline")
        .expect_err("and must not succeed");
        assert!(
            matches!(err, crate::error::BlinkError::Disconnected(_)),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn pipelined_download_upload_preserve_bytes() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));

        // Both files span more than the 16-wide in-flight window and are not
        // chunk-aligned, so ordered reassembly and the final partial chunk are
        // both exercised.
        let download_bytes = pseudo_random(17 * super::TRANSFER_CHUNK + 137);
        store
            .lock()
            .await
            .insert("/download.bin".to_string(), download_bytes.clone());

        let (port, _connects) = start_server(store.clone()).await;
        let mut transport = connect(port).await;

        // ---- download ----
        let dl_dst = std::env::temp_dir().join(format!("blink-it-dl-{port}.bin"));
        let _ = tokio::fs::remove_file(&dl_dst).await;
        let _ = tokio::fs::remove_file(crate::transport::part_path(&dl_dst)).await;

        transport
            .download("/download.bin", &dl_dst, None)
            .await
            .expect("download should succeed");
        let got = tokio::fs::read(&dl_dst).await.unwrap();
        assert_eq!(got.len(), download_bytes.len(), "downloaded size mismatch");
        assert!(got == download_bytes, "downloaded bytes mismatch");
        let _ = tokio::fs::remove_file(&dl_dst).await;

        // ---- upload ----
        let upload_bytes = pseudo_random(19 * super::TRANSFER_CHUNK + 4096);
        let ul_src = std::env::temp_dir().join(format!("blink-it-ul-{port}.bin"));
        tokio::fs::write(&ul_src, &upload_bytes).await.unwrap();

        transport
            .upload(&ul_src, "/uploaded.bin", None)
            .await
            .expect("upload should succeed");
        let stored = store
            .lock()
            .await
            .get("/uploaded.bin")
            .cloned()
            .expect("uploaded file should be present");
        assert_eq!(stored.len(), upload_bytes.len(), "uploaded size mismatch");
        assert!(stored == upload_bytes, "uploaded bytes mismatch");
        assert!(
            !store
                .lock()
                .await
                .contains_key(&crate::transport::remote_part_path("/uploaded.bin")),
            "the upload's partial must be renamed away after a successful upload"
        );

        // ---- upload over an existing remote file ----
        // The test server enforces SFTP v3 rename semantics (fails when the
        // target exists), so this exercises the move-aside-then-rename fallback
        // that finalizes an overwriting upload.
        let replacement = pseudo_random(3 * super::TRANSFER_CHUNK + 99);
        tokio::fs::write(&ul_src, &replacement).await.unwrap();
        transport
            .upload(&ul_src, "/uploaded.bin", None)
            .await
            .expect("overwriting upload should succeed");
        let stored = store
            .lock()
            .await
            .get("/uploaded.bin")
            .cloned()
            .expect("uploaded file should be present");
        assert!(stored == replacement, "overwritten bytes mismatch");
        assert!(
            !store
                .lock()
                .await
                .contains_key(&crate::transport::remote_part_path("/uploaded.bin")),
            "the upload's partial must be renamed away after an overwriting upload"
        );
        let _ = tokio::fs::remove_file(&ul_src).await;

        let _ = transport.close().await;
    }

    /// Bound a harness call, so a regression hangs for seconds rather than
    /// forever. Mirrors the FTP harness's helper of the same name.
    async fn with_timeout<F: std::future::Future>(fut: F, what: &str) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Previews read through russh-sftp's `File`, which since 3.0 probes the
    /// server's read size with one request and then keeps 16 in flight. The
    /// server caps every reply at `SHORT_READ`, so the probe comes back short
    /// and sets the size the pipeline uses; the file spans more than a full
    /// window of those and ends mid-chunk, so the last read is short too.
    /// SCP previews go through the same code, via `ScpTransport`.
    #[tokio::test]
    async fn a_preview_returns_every_byte() {
        let payload = pseudo_random(17 * SHORT_READ + 137);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/file.bin".to_string(), payload.clone());
        let (port, _connects) = start_server(store).await;
        let mut transport = connect(port).await;

        let got = with_timeout(transport.read_to_bytes("/file.bin"), "preview")
            .await
            .expect("a file under the cap should preview");
        assert_eq!(got.len(), payload.len(), "preview size mismatch");
        assert!(got[..] == payload[..], "preview bytes mismatch");
    }

    /// The cap is inclusive: `read_to_bytes` reads one byte past it to tell a
    /// file that exactly fits from one that does not.
    #[tokio::test]
    async fn a_preview_of_exactly_the_cap_succeeds() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store.lock().await.insert(
            "/full.bin".to_string(),
            vec![7u8; super::MAX_PREVIEW_BYTES as usize],
        );
        let (port, _connects) = start_server(store).await;
        let mut transport = connect(port).await;

        let got = with_timeout(transport.read_to_bytes("/full.bin"), "preview at the cap")
            .await
            .expect("a file of exactly the cap should preview");
        assert_eq!(got.len() as u64, super::MAX_PREVIEW_BYTES);
    }

    /// Hitting the cap is not a dropped connection, and it leaves the
    /// connection usable. The read stops at the cap with up to 16 requests
    /// still in flight and drops the `File`; their replies still arrive, so
    /// the listing and preview afterwards show nothing mistakes one for its
    /// own. The preview runs on the browsing connection, so both must work.
    /// Mirrors the FTP test of the same name.
    #[tokio::test]
    async fn an_oversized_preview_is_a_transport_error_and_the_connection_survives() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            // The harness only lists a directory that has its own key.
            files.insert("/".to_string(), Vec::new());
            files.insert(
                "/big.bin".to_string(),
                vec![7u8; super::MAX_PREVIEW_BYTES as usize + 1],
            );
            files.insert("/small.txt".to_string(), b"hello".to_vec());
        }
        let (port, _connects) = start_server(store).await;
        let mut transport = connect(port).await;

        let err = with_timeout(transport.read_to_bytes("/big.bin"), "oversized preview")
            .await
            .expect_err("a file one byte over the cap must not preview");
        assert!(
            matches!(err, crate::error::BlinkError::Transport(_)),
            "the cap is not a disconnect: {err:?}",
        );
        assert!(err.to_string().contains("preview size limit"), "{err}");

        let entries = with_timeout(transport.list("/"), "list after the preview")
            .await
            .expect("the connection must still serve a listing")
            .entries;
        let mut names: Vec<_> = entries.iter().map(|e| e.display_name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["big.bin", "small.txt"]);
        let small = with_timeout(transport.read_to_bytes("/small.txt"), "second preview")
            .await
            .expect("and a preview under the cap");
        assert_eq!(&small[..], b"hello");
    }

    /// A partial recorded for another server — same path, same size — must
    /// not be continued. The planted partial holds the wrong bytes, so a
    /// resume would show in the result.
    #[tokio::test]
    async fn a_partial_from_another_server_is_not_resumed() {
        let bytes = pseudo_random(3 * SHORT_READ);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/c.bin".to_string(), bytes.clone());
        let (port, _c) = start_server(store).await;
        let mut transport = connect(port).await;

        let dst = std::env::temp_dir().join(format!("blink-other-origin-{port}.bin"));
        let _ = std::fs::remove_file(&dst);
        std::fs::write(crate::transport::part_path(&dst), vec![0u8; 15_000]).unwrap();
        crate::transport::write_part_meta(
            &dst,
            "/c.bin",
            harness_stamp(bytes.len()),
            "tester@elsewhere.example:22",
        )
        .await;

        transport.download("/c.bin", &dst, None).await.unwrap();
        assert!(
            std::fs::read(&dst).unwrap() == bytes,
            "the download must start over, not continue another server's bytes",
        );
        let _ = std::fs::remove_file(&dst);
        let _ = transport.close().await;
    }

    /// Chunks are fetched in parallel and appended in order. One that ended
    /// early, with later chunks still returning data, had the rest appended
    /// straight after it: every byte past the gap landed at the wrong
    /// offset, the drain after the last chunk made the size come out right,
    /// and the scrambled file was renamed into place as a success.
    #[tokio::test]
    async fn a_download_that_ends_early_mid_file_fails() {
        let payload = pseudo_random(3 * super::TRANSFER_CHUNK);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/gap.bin".to_string(), payload.clone());
            // Inside the first chunk, ending at the second.
            let gap = format!("100000..{}", super::TRANSFER_CHUNK);
            files.insert(eof_gap_key("/gap.bin"), gap.into_bytes());
        }
        let (port, _c) = start_server(store).await;
        let mut transport = connect(port).await;

        let dst = std::env::temp_dir().join(format!("blink-eof-gap-{port}.bin"));
        let _ = std::fs::remove_file(&dst);
        let res = transport.download("/gap.bin", &dst, None).await;

        assert!(res.is_err(), "a download with a hole in it must fail");
        assert!(!dst.exists(), "and nothing is renamed into place");
        let _ = std::fs::remove_file(crate::transport::part_path(&dst));
        crate::transport::clear_part_meta(&dst).await;
        let _ = transport.close().await;
    }

    /// A rename the user confirmed as an overwrite. SFTP v3 rename, which
    /// OpenSSH's server implements, refuses a target that exists — as this
    /// harness does — so the confirmation promised what the rename then
    /// failed to do.
    #[tokio::test]
    async fn a_confirmed_rename_replaces_the_target() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/a.txt".to_string(), b"from a".to_vec());
            files.insert("/b.txt".to_string(), b"old b".to_vec());
        }
        let (port, _c) = start_server(Arc::clone(&store)).await;
        let mut transport = connect(port).await;

        transport.rename_over("/a.txt", "/b.txt").await.unwrap();

        let files = store.lock().await;
        assert_eq!(files.get("/b.txt").map(Vec::as_slice), Some(&b"from a"[..]));
        assert!(!files.contains_key("/a.txt"));
        assert!(
            !files.keys().any(|k| k.ends_with(".blink-old")),
            "the replaced file is removed once the rename lands: {:?}",
            files.keys()
        );
        drop(files);
        let _ = transport.close().await;
    }

    /// Making room for the rename deletes the target. With the source gone —
    /// renamed or deleted since the listing — that would destroy the target
    /// and then fail, losing the file the user meant to replace for nothing.
    #[tokio::test]
    async fn a_confirmed_rename_of_a_missing_file_keeps_the_target() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/b.txt".to_string(), b"old b".to_vec());
        let (port, _c) = start_server(Arc::clone(&store)).await;
        let mut transport = connect(port).await;

        assert!(transport.rename_over("/gone.txt", "/b.txt").await.is_err());

        assert_eq!(
            store.lock().await.get("/b.txt").map(Vec::as_slice),
            Some(&b"old b"[..])
        );
        let _ = transport.close().await;
    }

    // -- mkdir, delete_file and rename, against the server -----------------
    //
    // The TUI's tests run these against a mock; these are the calls
    // themselves, and how the server's answers are read.

    /// A server holding `/a.txt` and the folder `/dir`, and a connection.
    async fn serving_a_file_and_a_folder() -> (Store, SftpTransport) {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/a.txt".to_string(), b"a".to_vec());
            files.insert("/dir/".to_string(), Vec::new());
        }
        let (port, _c) = start_server(Arc::clone(&store)).await;
        (store, connect(port).await)
    }

    #[tokio::test]
    async fn mkdir_creates_the_folder() {
        let (store, mut transport) = serving_a_file_and_a_folder().await;

        transport.mkdir("/new").await.unwrap();

        assert!(store.lock().await.contains_key("/new/"));
        let made = transport.metadata("/new").await.unwrap().unwrap();
        assert!(made.is_dir());
        let _ = transport.close().await;
    }

    /// A recursive upload makes every folder on its way, most of which are
    /// there already. That isn't a failure, and the server isn't asked.
    #[tokio::test]
    async fn mkdir_of_a_folder_that_exists_succeeds() {
        let (_store, mut transport) = serving_a_file_and_a_folder().await;

        transport.mkdir("/dir").await.unwrap();
        transport.mkdir("/dir").await.unwrap();

        let _ = transport.close().await;
    }

    #[tokio::test]
    async fn mkdir_where_a_file_is_fails_and_leaves_the_file() {
        let (store, mut transport) = serving_a_file_and_a_folder().await;

        let err = transport.mkdir("/a.txt").await.unwrap_err();

        assert!(err.to_string().contains("not a directory"), "{err}");
        let files = store.lock().await;
        assert_eq!(files.get("/a.txt").map(Vec::as_slice), Some(&b"a"[..]));
        assert!(!files.contains_key("/a.txt/"));
        drop(files);
        let _ = transport.close().await;
    }

    #[tokio::test]
    async fn delete_file_removes_the_file_and_nothing_else() {
        let (store, mut transport) = serving_a_file_and_a_folder().await;

        transport.delete_file("/a.txt").await.unwrap();

        let files = store.lock().await;
        assert!(!files.contains_key("/a.txt"));
        assert!(files.contains_key("/dir/"));
        drop(files);
        let _ = transport.close().await;
    }

    /// Told apart from other failures, so a caller can treat "already
    /// gone" as it sees fit.
    #[tokio::test]
    async fn delete_file_of_a_missing_file_is_not_found() {
        let (_store, mut transport) = serving_a_file_and_a_folder().await;

        let err = transport.delete_file("/gone.txt").await.unwrap_err();

        assert!(
            matches!(err, crate::error::BlinkError::NotFound(_)),
            "{err}"
        );
        assert!(err.to_string().contains("/gone.txt"), "{err}");
        let _ = transport.close().await;
    }

    #[tokio::test]
    async fn rename_moves_the_file() {
        let (store, mut transport) = serving_a_file_and_a_folder().await;

        transport.rename("/a.txt", "/b.txt").await.unwrap();

        let files = store.lock().await;
        assert_eq!(files.get("/b.txt").map(Vec::as_slice), Some(&b"a"[..]));
        assert!(!files.contains_key("/a.txt"));
        drop(files);
        let _ = transport.close().await;
    }

    /// Plain rename is the one for a rename the user did not confirm as an
    /// overwrite: a target that turned up since the listing must stop it.
    #[tokio::test]
    async fn rename_onto_an_existing_file_fails_and_changes_nothing() {
        let (store, mut transport) = serving_a_file_and_a_folder().await;
        store
            .lock()
            .await
            .insert("/b.txt".to_string(), b"b".to_vec());

        let err = transport.rename("/a.txt", "/b.txt").await.unwrap_err();

        assert!(err.to_string().contains("/a.txt -> /b.txt"), "{err}");
        let files = store.lock().await;
        assert_eq!(files.get("/a.txt").map(Vec::as_slice), Some(&b"a"[..]));
        assert_eq!(files.get("/b.txt").map(Vec::as_slice), Some(&b"b"[..]));
        drop(files);
        let _ = transport.close().await;
    }

    #[tokio::test]
    async fn rename_of_a_missing_file_is_not_found() {
        let (_store, mut transport) = serving_a_file_and_a_folder().await;

        let err = transport.rename("/gone.txt", "/b.txt").await.unwrap_err();

        assert!(
            matches!(err, crate::error::BlinkError::NotFound(_)),
            "{err}"
        );
        let _ = transport.close().await;
    }

    /// Without `posix-rename`, replacing a file used to mean deleting it and
    /// renaming the new one into place. When that rename failed too, for
    /// whatever made the first one fail, the old file was already gone.
    #[tokio::test]
    async fn a_replacement_that_fails_keeps_the_old_file() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/b.txt".to_string(), b"old b".to_vec());
        let (port, _c) = start_server(Arc::clone(&store)).await;
        let mut transport = connect(port).await;
        let raw = Arc::clone(&transport.transfer_session().await.unwrap().raw);

        let replaced = super::finalize_remote_rename(&raw, false, "/gone.txt", "/b.txt").await;

        assert!(replaced.is_err());
        let files = store.lock().await;
        assert_eq!(files.get("/b.txt").map(Vec::as_slice), Some(&b"old b"[..]));
        assert!(
            !files.keys().any(|k| k.ends_with(".blink-old")),
            "the old file is put back, not left aside: {:?}",
            files.keys()
        );
        drop(files);
        let _ = transport.close().await;
    }

    /// A folder where the upload is going is not the upload's to replace.
    /// Moved aside like a file, it would stay there: the fallback removes
    /// what it moved aside as a file, and a folder can't be.
    #[tokio::test]
    async fn an_upload_onto_a_folder_leaves_the_folder_alone() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/up.bin/".to_string(), Vec::new());
            files.insert("/up.bin/inner.txt".to_string(), b"mine".to_vec());
            files.insert("/up.bin.blink-part".to_string(), b"new".to_vec());
        }
        let (port, _c) = start_server(Arc::clone(&store)).await;
        let mut transport = connect(port).await;
        let raw = Arc::clone(&transport.transfer_session().await.unwrap().raw);

        let replaced =
            super::finalize_remote_rename(&raw, false, "/up.bin.blink-part", "/up.bin").await;

        assert!(replaced.is_err());
        let files = store.lock().await;
        assert!(files.contains_key("/up.bin/"), "{:?}", files.keys());
        assert_eq!(
            files.get("/up.bin/inner.txt").map(Vec::as_slice),
            Some(&b"mine"[..])
        );
        assert!(
            !files.keys().any(|k| k.contains(".blink-old")),
            "nothing is moved aside: {:?}",
            files.keys()
        );
        drop(files);
        let _ = transport.close().await;
    }

    /// The harness itself: it renames folders, so the test above shows the
    /// guard, not the server, keeping the folder in place.
    #[tokio::test]
    async fn the_harness_renames_a_folder_with_its_contents() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/a/".to_string(), Vec::new());
            files.insert("/a/f.txt".to_string(), b"f".to_vec());
        }
        let (port, _c) = start_server(Arc::clone(&store)).await;
        let mut transport = connect(port).await;

        transport.rename("/a", "/b").await.unwrap();

        let files = store.lock().await;
        let mut keys: Vec<&String> = files.keys().collect();
        keys.sort();
        assert_eq!(keys, ["/b/", "/b/f.txt"]);
        drop(files);
        let _ = transport.close().await;
    }

    /// Somebody else's `x.part` — a browser's, another tool's — sitting
    /// beside a download of `x` used to be deleted (it could not be
    /// identified as a partial of `x`) or truncated. blink's partials now
    /// have a name of their own, and a foreign `.part` is none of its
    /// business.
    #[tokio::test]
    async fn a_download_leaves_another_tools_part_file_alone() {
        let payload = pseudo_random(3 * SHORT_READ);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/report.pdf".to_string(), payload.clone());
        let (port, _c) = start_server(store).await;
        let mut transport = connect(port).await;

        let dir = std::env::temp_dir().join(format!("blink-foreign-part-{port}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let local = dir.join("report.pdf");
        let theirs = dir.join("report.pdf.part");
        std::fs::write(&theirs, b"someone else's partial").unwrap();

        transport
            .download("/report.pdf", &local, None)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&local).unwrap(), payload);
        assert_eq!(
            std::fs::read(&theirs).unwrap(),
            b"someone else's partial",
            "a file blink did not write must survive",
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = transport.close().await;
    }

    /// The remote mirror: an upload of `x` opened `x.part` with TRUNCATE and
    /// removed it on failure, whoever it belonged to.
    #[tokio::test]
    async fn an_upload_leaves_a_remote_part_file_alone() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/data.csv.part".to_string(), b"theirs".to_vec());
        let (port, _c) = start_server(store.clone()).await;
        let mut transport = connect(port).await;

        let src = std::env::temp_dir().join(format!("blink-upload-src-{port}.csv"));
        std::fs::write(&src, b"a,b\n1,2\n").unwrap();
        transport.upload(&src, "/data.csv", None).await.unwrap();

        let files = store.lock().await;
        assert_eq!(
            files.get("/data.csv").map(Vec::as_slice),
            Some(&b"a,b\n1,2\n"[..])
        );
        assert_eq!(
            files.get("/data.csv.part").map(Vec::as_slice),
            Some(&b"theirs"[..]),
            "a remote file blink did not write must survive",
        );
        drop(files);
        let _ = std::fs::remove_file(&src);
        let _ = transport.close().await;
    }

    /// A `.blink-part` left behind by a download of a *different* remote file must
    /// never be resumed into the current one. Before the provenance check,
    /// this appended B's tail to A's head and renamed the result into place
    /// as a completed download of B.
    #[tokio::test]
    async fn a_partial_from_another_file_is_not_resumed_into_this_one() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let a_bytes = pseudo_random(40_000);
        let b_bytes = pseudo_random(52_000)
            .into_iter()
            .map(|b| b ^ 0xA5) // make B unmistakably different from A
            .collect::<Vec<u8>>();
        store
            .lock()
            .await
            .insert("/a.bin".to_string(), a_bytes.clone());
        store
            .lock()
            .await
            .insert("/b.bin".to_string(), b_bytes.clone());

        let (port, _c) = start_server(store).await;
        let mut transport = connect(port).await;

        let dst = std::env::temp_dir().join(format!("blink-prov-{port}.bin"));
        let part = crate::transport::part_path(&dst);
        let meta = crate::transport::part_meta_path(&dst);
        for f in [&dst, &part, &meta] {
            let _ = tokio::fs::remove_file(f).await;
        }

        // Simulate an interrupted download of /a.bin: a partial plus the
        // sidecar naming where those bytes came from.
        tokio::fs::write(&part, &a_bytes[..10_000]).await.unwrap();
        crate::transport::write_part_meta(
            &dst,
            "/a.bin",
            harness_stamp(a_bytes.len()),
            &crate::transport::origin_of(&test_session(port)),
        )
        .await;

        // Now download a different remote file to the same local name.
        transport
            .download("/b.bin", &dst, None)
            .await
            .expect("download should succeed");

        let got = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(
            got.len(),
            b_bytes.len(),
            "the stale partial must not have been prepended",
        );
        assert!(got == b_bytes, "downloaded bytes must be exactly /b.bin");
        assert!(
            !tokio::fs::try_exists(&meta).await.unwrap_or(false),
            "the sidecar must be cleared once the download lands",
        );

        for f in [&dst, &part, &meta] {
            let _ = tokio::fs::remove_file(f).await;
        }
        let _ = transport.close().await;
    }

    /// The other half: a partial that *is* this file must still be resumed,
    /// or the provenance check would have turned resume off entirely.
    #[tokio::test]
    async fn a_partial_of_this_file_is_resumed_and_completes_correctly() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let bytes = pseudo_random(40_000);
        store
            .lock()
            .await
            .insert("/c.bin".to_string(), bytes.clone());

        let (port, _c) = start_server(store).await;
        let mut transport = connect(port).await;

        let dst = std::env::temp_dir().join(format!("blink-prov-ok-{port}.bin"));
        let part = crate::transport::part_path(&dst);
        let meta = crate::transport::part_meta_path(&dst);
        for f in [&dst, &part, &meta] {
            let _ = tokio::fs::remove_file(f).await;
        }

        // The partial's bytes are marked (inverted), so the result shows
        // whether they were kept: a download that restarted from zero would
        // land the source unchanged and pass a plain equality check.
        let head: Vec<u8> = bytes[..15_000].iter().map(|b| !b).collect();
        tokio::fs::write(&part, &head).await.unwrap();
        crate::transport::write_part_meta(
            &dst,
            "/c.bin",
            harness_stamp(bytes.len()),
            &crate::transport::origin_of(&test_session(port)),
        )
        .await;

        transport
            .download("/c.bin", &dst, None)
            .await
            .expect("download should succeed");

        let got = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(got.len(), bytes.len(), "resumed file has the wrong length");
        assert!(
            got[..15_000] == head[..],
            "the partial must be continued, not restarted"
        );
        assert!(
            got[15_000..] == bytes[15_000..],
            "the tail must come from the source at the offset"
        );

        for f in [&dst, &part, &meta] {
            let _ = tokio::fs::remove_file(f).await;
        }
        let _ = transport.close().await;
    }

    /// The dispatcher's connection pool must reuse one connection across
    /// sequential jobs instead of opening a fresh SSH session per job.
    /// Two downloads at parallelism 1 run strictly back-to-back, so the
    /// server must see exactly one TCP connection.
    #[tokio::test]
    async fn dispatcher_reuses_pooled_connection_across_jobs() {
        use crate::transfer::{Dispatcher, TransferEvent, TransferManager, TransferState};
        use crate::tui::event::AppEvent;

        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/a.bin".to_string(), pseudo_random(100_000));
        store
            .lock()
            .await
            .insert("/b.bin".to_string(), pseudo_random(100_000));

        let (port, connects) = start_server(store).await;

        let session = Session {
            name: "disp".to_string(),
            protocol: Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        };

        let dir = std::env::temp_dir().join(format!("blink-disp-{port}"));
        let _ = tokio::fs::create_dir_all(&dir).await;
        let a_dst = dir.join("a.bin");
        let b_dst = dir.join("b.bin");
        for f in [&a_dst, &b_dst] {
            let _ = tokio::fs::remove_file(f).await;
            let _ = tokio::fs::remove_file(crate::transport::part_path(f)).await;
        }

        let (manager, mut events_rx) = TransferManager::new(1);
        manager
            .enqueue_download("/a.bin".to_string(), a_dst.clone())
            .expect("queue a");
        manager
            .enqueue_download("/b.bin".to_string(), b_dst.clone())
            .expect("queue b");

        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
        let dispatcher = Dispatcher::spawn(
            manager.clone(),
            session,
            Some(zeroize::Zeroizing::new("pw".to_string())),
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
        );

        // Drive both event streams: answer host-key prompts (the ephemeral
        // test host is always unknown) and count completed transfers.
        let mut completed = 0usize;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    Some(ev) = ev_rx.recv() => {
                        if let AppEvent::HostKeyUnknown { decision_tx, .. } = ev {
                            let _ = decision_tx.send(HostKeyDecision::AcceptOnce);
                        }
                    }
                    Some(te) = events_rx.recv() => match te {
                        TransferEvent::Complete(_) => {
                            completed += 1;
                            if completed == 2 {
                                break;
                            }
                        }
                        TransferEvent::Failed { error, .. } => {
                            panic!("transfer failed: {error}");
                        }
                        _ => {}
                    },
                }
            }
        })
        .await
        .expect("both downloads should complete within 30s");

        dispatcher.shutdown().await;

        assert_eq!(
            connects.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second job should reuse the pooled connection, not reconnect"
        );

        // Every dispatched job must land in a terminal state. A job left
        // Active after shutdown means its worker returned without marking it
        // — the symptom of the abort-handle registration race.
        for job in manager.snapshot() {
            assert!(
                matches!(job.state, TransferState::Complete),
                "job {} left in non-terminal state {:?}",
                job.id,
                job.state
            );
        }

        for f in [&a_dst, &b_dst] {
            let _ = tokio::fs::remove_file(f).await;
        }
    }

    /// Build a self-signed SSH *host* certificate. Nothing validates it — the
    /// point is only that the callback receives the `Certificate` variant.
    /// `Builder::sign` refuses an empty principal list ("golden ticket"), so
    /// `all_principals_valid` is set explicitly.
    fn host_certificate() -> russh::keys::ssh_key::Certificate {
        use russh::keys::ssh_key::certificate::{Builder, CertType};

        let ca = russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
            .unwrap();
        let subject =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();

        let mut builder = Builder::new(
            [0u8; 16],
            subject.public_key().key_data().clone(),
            0,
            u64::MAX >> 1,
        )
        .unwrap();
        builder.cert_type(CertType::Host).unwrap();
        builder.all_principals_valid().unwrap();
        builder.key_id("blink-test").unwrap();
        builder.sign(&ca).unwrap()
    }

    /// A server presenting an SSH host *certificate* must be refused. blink's
    /// known_hosts stores host -> (key type, base64 key) and has no
    /// @cert-authority support, so it cannot validate a certificate's CA
    /// signature, principals, or validity window. TOFU-pinning one would look
    /// like verification while checking none of that; russh 0.60 could not
    /// surface a certificate here at all, so refusing preserves behaviour.
    #[tokio::test]
    async fn a_host_certificate_is_refused() {
        use russh::client::Handler as _;
        use russh::keys::PublicKeyOrCertificate;

        let (ev_tx, mut ev_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::tui::event::AppEvent>();
        let mut handler = super::KnownHostsHandler {
            host: "example.test".to_string(),
            port: 22,
            event_tx: Some(ev_tx),
            trust: crate::known_hosts::SessionTrust::new(),
            user_wait: crate::transport::UserWait::new().0,
        };

        let accepted = handler
            .check_server_key(&PublicKeyOrCertificate::Certificate(host_certificate()))
            .await
            .expect("the callback itself must not error");

        assert!(!accepted, "a host certificate must be refused");

        let ev = ev_rx
            .try_recv()
            .expect("the rejection must reach the TUI, not just the log");
        match ev {
            crate::tui::event::AppEvent::HostCertificateRejected { host } => {
                assert_eq!(host, "example.test");
            }
            // Naming the wrong variant matters more than counting events,
            // and `AppEvent` is deliberately not `Debug`, so spell out the
            // one variant that must never appear here.
            crate::tui::event::AppEvent::HostKeyUnknown { .. } => {
                panic!("a certificate must not raise the trust-on-first-use prompt")
            }
            _ => panic!("expected AppEvent::HostCertificateRejected"),
        }
    }

    /// An unknown key with nobody answering yet: the handler under test,
    /// the key it was shown, and the channels around it.
    fn prompting_handler() -> (
        super::KnownHostsHandler,
        russh::keys::PublicKeyOrCertificate,
        tokio::sync::mpsc::UnboundedReceiver<crate::tui::event::AppEvent>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (user_wait, waiting) = crate::transport::UserWait::new();
        let handler = super::KnownHostsHandler {
            host: "example.test".to_string(),
            port: 22,
            event_tx: Some(ev_tx),
            trust: crate::known_hosts::SessionTrust::new(),
            user_wait,
        };
        let key = russh::keys::PrivateKey::from_openssh(super::super::sftp_test_keys::ED25519_KEY)
            .unwrap()
            .public_key()
            .clone();
        let presented = russh::keys::PublicKeyOrCertificate::PublicKey {
            key,
            hash_alg: None,
        };
        (handler, presented, ev_rx, waiting)
    }

    /// The connect deadline must not run while the user reads a fingerprint,
    /// so the handler says when it is waiting on them — and stops saying so
    /// once they answer.
    #[tokio::test]
    async fn the_handler_signals_while_the_prompt_waits() {
        use russh::client::Handler as _;
        let _home = crate::paths::test_home();
        let (mut handler, presented, mut ev_rx, mut waiting) = prompting_handler();

        let check = tokio::spawn(async move { handler.check_server_key(&presented).await });
        let Some(crate::tui::event::AppEvent::HostKeyUnknown { decision_tx, .. }) =
            ev_rx.recv().await
        else {
            panic!("an unknown key must raise the prompt");
        };
        assert!(
            *waiting.borrow_and_update(),
            "the prompt is open, so the deadline must be paused",
        );

        let _ = decision_tx.send(super::HostKeyDecision::AcceptOnce);
        assert!(check.await.unwrap().unwrap(), "accepted once");
        assert!(!*waiting.borrow(), "answered, so the deadline runs again");
    }

    /// Nobody answering is not an open-ended pause: the prompt expires on
    /// its own limit, rejects the key, and ends the pause.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_prompt_expires_and_ends_the_pause() {
        use russh::client::Handler as _;
        let _home = crate::paths::test_home();
        let (mut handler, presented, mut ev_rx, waiting) = prompting_handler();

        let started = tokio::time::Instant::now();
        let check = tokio::spawn(async move { handler.check_server_key(&presented).await });
        // Hold the decision sender unanswered: dropping it would read as an
        // immediate reject rather than as a user who never answers.
        let _unanswered = match ev_rx.recv().await {
            Some(crate::tui::event::AppEvent::HostKeyUnknown { decision_tx, .. }) => decision_tx,
            _ => panic!("an unknown key must raise the prompt"),
        };

        assert!(!check.await.unwrap().unwrap(), "an expired prompt rejects");
        assert_eq!(
            started.elapsed(),
            super::HOST_KEY_DECISION_TIMEOUT,
            "it waits the prompt's own limit, no less",
        );
        assert!(!*waiting.borrow(), "and ends the pause");
    }

    /// A key on a `@revoked` line is refused without a prompt, even when the
    /// user accepted it once this session, and the TUI is told why.
    #[tokio::test]
    async fn a_revoked_key_is_refused_without_a_prompt() {
        use base64::Engine as _;
        use russh::client::Handler as _;
        let _home = crate::paths::test_home();
        let (mut handler, presented, mut ev_rx, _waiting) = prompting_handler();
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = &presented else {
            unreachable!()
        };
        let key_type = key.algorithm().as_str().to_string();
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.to_bytes().unwrap());
        std::fs::write(
            crate::known_hosts::known_hosts_path().unwrap(),
            format!("@revoked * {key_type} {b64}\n"),
        )
        .unwrap();
        handler.trust.trust("example.test", 22, &key_type, &b64);

        assert!(!handler.check_server_key(&presented).await.unwrap());
        match ev_rx.try_recv() {
            Ok(crate::tui::event::AppEvent::HostKeyRevoked { host, .. }) => {
                assert_eq!(host, "example.test");
            }
            _ => panic!("expected AppEvent::HostKeyRevoked"),
        }
    }

    // ---- ssh-agent login --------------------------------------------------
    //
    // `try_agent_identities` is what every platform's agent branch runs, so
    // these pin how an agent login fails and succeeds without a real agent:
    // russh's own agent server, over an in-memory pipe.

    /// An agent holding `keys` (OpenSSH private keys).
    async fn agent_holding(
        keys: &[&str],
    ) -> russh::keys::agent::client::AgentClient<tokio::io::DuplexStream> {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let listener = futures::stream::iter([Ok(server_end)]);
        tokio::spawn(russh::keys::agent::server::serve(listener, ()));
        let mut agent = russh::keys::agent::client::AgentClient::connect(client_end);
        for k in keys {
            let key = russh::keys::PrivateKey::from_openssh(k).unwrap();
            agent.add_identity(&key, &[]).await.unwrap();
        }
        agent
    }

    /// A client handle to the test server on `port`, host key verified (as
    /// trusted once) but not yet authenticated.
    async fn unauthenticated_handle(port: u16) -> russh::client::Handle<super::KnownHostsHandler> {
        use base64::Engine as _;
        let host_key =
            russh::keys::PrivateKey::from_openssh(super::super::sftp_test_keys::ED25519_KEY)
                .unwrap()
                .public_key()
                .clone();
        let trust = crate::known_hosts::SessionTrust::new();
        trust.trust(
            "127.0.0.1",
            port,
            host_key.algorithm().as_str(),
            &base64::engine::general_purpose::STANDARD.encode(host_key.to_bytes().unwrap()),
        );
        let handler = super::KnownHostsHandler {
            host: "127.0.0.1".to_string(),
            port,
            event_tx: None,
            trust,
            user_wait: crate::transport::UserWait::new().0,
        };
        let config = Arc::new(russh::client::Config::default());
        russh::client::connect(config, ("127.0.0.1", port), handler)
            .await
            .expect("connect to the test server")
    }

    #[tokio::test]
    async fn an_agent_login_succeeds_with_a_key_the_server_accepts() {
        let _home = crate::paths::test_home();
        let (port, _c) = start_server(Arc::new(Mutex::new(HashMap::new()))).await;
        let mut handle = unauthenticated_handle(port).await;
        let mut agent = agent_holding(&[super::super::sftp_test_keys::ED25519_KEY]).await;

        super::try_agent_identities(&mut handle, "tester", &mut agent)
            .await
            .expect("the agent's key is accepted");
    }

    #[tokio::test]
    async fn an_empty_agent_says_so_and_how_to_fix_it() {
        let _home = crate::paths::test_home();
        let (port, _c) = start_server(Arc::new(Mutex::new(HashMap::new()))).await;
        let mut handle = unauthenticated_handle(port).await;
        let mut agent = agent_holding(&[]).await;

        let err = super::try_agent_identities(&mut handle, "tester", &mut agent)
            .await
            .expect_err("nothing to offer");
        assert!(err.contains("no identities loaded"), "{err}");
        assert!(err.contains(super::AGENT_EMPTY_HINT), "{err}");
    }

    /// Every key is offered before giving up, and the failure says that it
    /// was the keys, not the agent, that didn't work.
    #[tokio::test]
    async fn an_agent_login_reports_when_every_key_is_refused() {
        let _home = crate::paths::test_home();
        let port = start_server_rejecting_keys(Arc::new(Mutex::new(HashMap::new()))).await;
        let mut handle = unauthenticated_handle(port).await;
        let mut agent = agent_holding(&[
            super::super::sftp_test_keys::ED25519_KEY,
            super::super::sftp_test_keys::ECDSA_KEY,
        ])
        .await;

        let err = super::try_agent_identities(&mut handle, "tester", &mut agent)
            .await
            .expect_err("the server refuses every key");
        assert!(err.starts_with("ssh-agent: no identity accepted"), "{err}");
    }
}
