//! SFTP transport built on russh + russh-sftp.
//!
//! NOTE: russh and russh-sftp evolve their APIs across minor versions. The
//! shape below targets `russh` 0.60.x and `russh-sftp` 2.3.x. If a `cargo build`
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

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use russh::client::{self, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{load_secret_key, ssh_key};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::{self, BlinkError, Result};
use crate::known_hosts::{self, KeyStatus};
use crate::session::{AuthMethod, Protocol, Session};
use crate::transport::error_map::map_sftp;
use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry, Transport};

/// Cap on bytes read by `read_to_bytes` — matches the image preview limit.
const MAX_PREVIEW_BYTES: u64 = 10_000_000; // 10 MB

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
}

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
        server_public_key: &ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // Sanitize before any use: a server claiming a non-standard algorithm
        // name could inject ANSI sequences into the host-key modal and log.
        let key_type = error::sanitize(server_public_key.algorithm().as_str().to_string());
        let key_b64 = {
            use base64::Engine as _;
            let wire = server_public_key.to_bytes().unwrap_or_default();
            base64::engine::general_purpose::STANDARD.encode(&wire)
        };

        let fingerprint = error::sanitize(
            server_public_key
                .fingerprint(ssh_key::HashAlg::Sha256)
                .to_string(),
        );

        let display_host = self.display_host();

        match known_hosts::check(&self.host, self.port, &key_type, &key_b64) {
            Ok(KeyStatus::Trusted) => return Ok(true),
            Ok(KeyStatus::Changed { stored_key_type, .. }) => {
                tracing::warn!(
                    host = %display_host,
                    stored = %stored_key_type,
                    presented = %key_type,
                    "host key mismatch — rejecting connection",
                );
                // Send the changed-key event so the TUI can surface a clear
                // error message rather than a generic connect failure.
                let event = crate::tui::event::AppEvent::HostKeyChanged {
                    host: display_host,
                    stored_key_type,
                    presented_key_type: key_type,
                    fingerprint,
                };
                if let Some(tx) = self.event_tx.take() {
                    let _ = tx.send(event);
                }
                return Ok(false);
            }
            Err(e) => {
                // Fail closed: if we cannot read the known-hosts file we
                // cannot verify the host key, so reject the connection rather
                // than prompting the user (which would be fail-open).
                tracing::error!(
                    host = %display_host,
                    "known_hosts read error — rejecting connection: {e}"
                );
                return Ok(false);
            }
            Ok(KeyStatus::Unknown) => {}
        }

        // Unknown key: send the details to the TUI and await the user's call.
        let (decision_tx, decision_rx) = oneshot::channel();

        let event = crate::tui::event::AppEvent::HostKeyUnknown {
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

        // The TUI must respond within 60 seconds, otherwise reject
        // to avoid hanging the connection indefinitely.
        let decision = match tokio::time::timeout(
            std::time::Duration::from_secs(60),
            decision_rx,
        )
        .await
        {
            Ok(d) => d.unwrap_or(HostKeyDecision::Reject),
            Err(_) => {
                tracing::warn!(
                    host = %self.host,
                    "host-key decision timed out — rejecting"
                );
                HostKeyDecision::Reject
            }
        };

        match decision {
            HostKeyDecision::AcceptAndSave => {
                if let Err(e) = known_hosts::append(&self.host, self.port, &key_type, &key_b64) {
                    tracing::warn!("could not save host key: {e}");
                }
                Ok(true)
            }
            HostKeyDecision::AcceptOnce => Ok(true),
            HostKeyDecision::Reject => Ok(false),
        }
    }
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

/// Attempt authentication against every identity in the agent.
/// Returns `Ok(true)` on success, `Ok(false)` if no identity was accepted.
/// Using a generic `S` avoids `Box<dyn AgentStream>` in the state machine,
/// which lets the Rust compiler verify the `Send` bound for all code paths.
#[cfg(windows)]
async fn try_agent_identities<S>(
    handle: &mut Handle<KnownHostsHandler>,
    username: &str,
    agent: &mut russh::keys::agent::client::AgentClient<S>,
) -> Result<bool>
where
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
        .map_err(|e| BlinkError::auth(format!("ssh-agent request_identities: {e}")))?;
    if identities.is_empty() {
        return Err(BlinkError::auth(
            "ssh-agent has no identities loaded (try `ssh-add` or load keys into Pageant)",
        ));
    }

    for identity in identities {
        // See the unix branch: identities may be keys or certificates.
        let pubkey = identity.public_key().into_owned();
        let hash_alg = rsa_hash_alg(&pubkey.algorithm());
        match handle
            .authenticate_publickey_with(username, pubkey, hash_alg, agent)
            .await
        {
            Ok(r) if r.success() => return Ok(true),
            Ok(_) => {}
            Err(e) => {
                tracing::debug!("ssh-agent identity rejected: {e}");
            }
        }
    }
    Ok(false)
}

pub struct SftpTransport {
    handle: Handle<KnownHostsHandler>,
    sftp: SftpSession,
    /// Lazily-opened second SFTP channel used solely for pipelined byte
    /// transfers; all control ops stay on `sftp`. Opened on the first
    /// download/upload and reused for the connection's lifetime.
    transfer: Option<Transfer>,
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

impl SftpTransport {
    pub async fn connect(
        session: &Session,
        password: Option<&str>,
        app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    ) -> Result<Self> {
        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE_INTERVAL),
            keepalive_max: KEEPALIVE_MAX,
            window_size: CHANNEL_WINDOW,
            ..client::Config::default()
        });
        let addr = format!("{}:{}", session.host, session.port);

        let handler = KnownHostsHandler {
            host: session.host.clone(),
            port: session.port,
            event_tx: Some(app_event_tx),
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
                let kp = match load_secret_key(path, passphrase) {
                    Ok(k) => k,
                    Err(e) => {
                        let msg = e.to_string().to_lowercase();
                        if msg.contains("encrypted")
                            || msg.contains("passphrase")
                            || msg.contains("decrypt")
                        {
                            return Err(BlinkError::KeyNeedsPassphrase);
                        }
                        return Err(BlinkError::auth(format!(
                            "load key {}: {e}",
                            path.display()
                        )));
                    }
                };
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
                    let mut agent =
                        russh::keys::agent::client::AgentClient::connect_env()
                            .await
                            .map_err(|e| {
                                BlinkError::auth(format!("ssh-agent connect: {e}"))
                            })?;

                    let identities = agent.request_identities().await.map_err(|e| {
                        BlinkError::auth(format!("ssh-agent request_identities: {e}"))
                    })?;
                    if identities.is_empty() {
                        return Err(BlinkError::auth(
                            "ssh-agent has no identities loaded (try `ssh-add`)",
                        ));
                    }

                    let mut succeeded = false;
                    let mut last_err: Option<String> = None;
                    for identity in identities {
                        // russh 0.60 models agent identities as plain keys OR
                        // OpenSSH certificates; `public_key` normalises both.
                        let pubkey = identity.public_key().into_owned();
                        let hash_alg = rsa_hash_alg(&pubkey.algorithm());
                        let auth_result = handle
                            .authenticate_publickey_with(
                                username, pubkey, hash_alg, &mut agent,
                            )
                            .await;
                        match auth_result {
                            Ok(r) if r.success() => {
                                succeeded = true;
                                break;
                            }
                            Ok(_) => {}
                            Err(e) => last_err = Some(e.to_string()),
                        }
                    }
                    if !succeeded {
                        return Err(BlinkError::auth(format!(
                            "ssh-agent: no identity accepted{}",
                            last_err
                                .map(|e| format!(" (last error: {e})"))
                                .unwrap_or_default()
                        )));
                    }
                    true
                }
                #[cfg(windows)]
                {
                    use russh::keys::agent::client::AgentClient;

                    const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

                    let succeeded =
                        match AgentClient::connect_named_pipe(OPENSSH_PIPE).await {
                            Ok(mut agent) => {
                                try_agent_identities(&mut handle, username, &mut agent)
                                    .await?
                            }
                            Err(pipe_err) => {
                                let mut agent = AgentClient::connect_pageant().await;
                                try_agent_identities(&mut handle, username, &mut agent)
                                    .await
                                    .map_err(|e| {
                                        BlinkError::auth(format!(
                                            "ssh-agent: no agent found \
                                             (OpenSSH pipe error: {pipe_err}; {e})"
                                        ))
                                    })?
                            }
                        };

                    if !succeeded {
                        return Err(BlinkError::auth(
                            "ssh-agent: no identity accepted",
                        ));
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
}

/// Read the byte range `[offset, offset + len)` fully into a `Vec`, looping to
/// absorb short reads (SFTP servers may return fewer bytes than requested).
/// A reply shorter than `len` that isn't itself short — or an `Eof` status —
/// marks end-of-file, so the returned `Vec` may be shorter than `len` only at
/// the end of the file.
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
                if data.data.is_empty() {
                    break;
                }
                let n = data.data.len();
                buf.extend_from_slice(&data.data);
                pos += n as u64;
                remaining = remaining.saturating_sub(n as u32);
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
    local: &mut tokio::fs::File,
    total: u64,
    progress: &Option<mpsc::UnboundedSender<ProgressUpdate>>,
) -> Result<()> {
    let mut done = start;

    if let Some(end) = size {
        let mut stream = futures::stream::iter(chunk_offsets(start, end, chunk))
            .map(|(off, len)| {
                let raw = Arc::clone(raw);
                let handle = handle.to_string();
                let label = label.to_string();
                async move { read_full(&raw, &handle, &label, off, len).await }
            })
            .buffered(concurrency);

        while let Some(chunk) = stream.next().await {
            let data = chunk?;
            if data.is_empty() {
                break;
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
        let data = read_full(raw, handle, label, done, chunk as u32).await?;
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
    // final path as if the download had succeeded; the stale `.part` is
    // detected and discarded on the next attempt.
    if let Some(end) = size
        && done < end {
            return Err(BlinkError::transport(format!(
                "{label}: remote file truncated during transfer \
                 (expected {end} bytes, got {done})"
            )));
        }
    Ok(())
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
/// no-target case), and when it's refused, remove the target and rename
/// again. That fallback has a non-atomic window, but it only ever exposes
/// "old file present" or "old file gone, complete new file at `part`" —
/// never a truncated file under the final name.
async fn finalize_remote_rename(
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
            Ok(Packet::Status(s)) => {
                Err(map_sftp("posix-rename", dest, SftpError::Status(s)))
            }
            Ok(_) => Err(BlinkError::transport(format!(
                "posix-rename {dest}: unexpected reply packet"
            ))),
            Err(e) => Err(map_sftp("posix-rename", dest, e)),
        };
    }

    if raw.rename(part, dest).await.is_ok() {
        return Ok(());
    }
    match raw.remove(dest).await {
        Ok(_) => {}
        Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {}
        Err(e) => return Err(map_sftp("remove", dest, e)),
    }
    raw.rename(part, dest)
        .await
        .map(|_| ())
        .map_err(|e| map_sftp("rename", dest, e))
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

        if let Some(res) = inflight.next().await {
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

    async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>> {
        let entries = self
            .sftp
            .read_dir(remote_path)
            .await
            .map_err(|e| map_sftp("readdir", remote_path, e))?;

        let mut out = Vec::new();
        for e in entries {
            let name = error::sanitize(e.file_name());
            if name == "." || name == ".." {
                continue;
            }
            let attrs = e.metadata();
            let kind = if attrs.is_dir() {
                EntryKind::Directory
            } else if attrs.is_symlink() {
                EntryKind::Symlink
            } else if attrs.is_regular() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            out.push(RemoteEntry {
                name,
                kind,
                size: attrs.size.unwrap_or(0),
                modified: attrs
                    .mtime
                    .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0)),
                mode: attrs.permissions,
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
        // Stream into `<local>.part` and rename onto the final path only on
        // success. This keeps the user's existing file (if any) untouched
        // until the download is fully complete and fsynced, and isolates
        // partial bytes from a previous attempt under a recognisable suffix.
        let part = super::part_path(local_path);

        // Resume support: if a partial `.part` exists, skip the bytes it
        // already holds so interrupted transfers pick up where they left off.
        let existing = tokio::fs::metadata(&part)
            .await
            .ok()
            .map(|m| m.len())
            .unwrap_or(0);

        let xfer = self.transfer_session().await?;
        let raw = Arc::clone(&xfer.raw);
        let read_chunk = xfer.read_chunk;

        let remote_handle = raw
            .open(remote_path, OpenFlags::READ, FileAttributes::default())
            .await
            .map_err(|e| map_sftp("open", remote_path, e))?
            .handle;

        let reported_size = raw
            .fstat(&remote_handle)
            .await
            .ok()
            .and_then(|a| a.attrs.size);

        let total = reported_size.unwrap_or(0);

        // If the server reports the file size and the partial file is already
        // larger, the file must have been replaced — restart from zero so we
        // don't append garbage.  If the server does not report a size (None),
        // we have no basis for comparison and trust the existing offset; not
        // resetting avoids silently restarting every resumed FTP-style transfer.
        let offset = match reported_size {
            Some(server_size) if existing > server_size => {
                tracing::warn!(
                    remote = %remote_path,
                    local_bytes = existing,
                    server_bytes = server_size,
                    "partial file is larger than server file — restarting download",
                );
                // Drop the stale `.part` so the OpenOptions below truncate
                // from a clean state.
                let _ = tokio::fs::remove_file(&part).await;
                0
            }
            _ => existing,
        };

        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut local = if offset > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .await?
        } else {
            tokio::fs::File::create(&part).await?
        };

        let result = pipelined_download(
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
        .await;
        let _ = raw.close(&remote_handle).await;
        result?;

        // Flush + fsync the .part so its bytes are durable, then atomically
        // rename onto the final path. Drop the handle first; renaming an
        // open file is fine on Unix but tokio's tempfile/rename pairing is
        // simpler when the source is closed.
        local.flush().await?;
        local.sync_all().await?;
        drop(local);
        tokio::fs::rename(&part, local_path).await?;
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

        // Stream into `<remote>.part` and rename onto the final name only
        // on success — the remote mirror of the download path. An
        // interrupted or failed upload must never leave a truncated file
        // under the destination name.
        let part = format!("{remote_path}.part");

        let remote_handle = raw
            .open(
                &part,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                FileAttributes::default(),
            )
            .await
            .map_err(|e| map_sftp("open", &part, e))?
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
            let _ = raw.fsync(&remote_handle).await;
        }
        let _ = raw.close(&remote_handle).await;

        match result {
            Ok(()) => finalize_remote_rename(&raw, posix_rename, &part, remote_path).await,
            Err(e) => {
                // Uploads don't resume, so a partial has no future value —
                // remove it best-effort so failed uploads don't litter the
                // server. (Pointless on Disconnected, harmless to try.)
                let _ = raw.remove(&part).await;
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

    async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
        self.sftp
            .remove_file(remote_path)
            .await
            .map_err(|e| map_sftp("remove", remote_path, e))
    }

    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
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
            match op {
                Op::Visit(path) => {
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
                        let child = super::join_remote(&path, &name);
                        let attrs = e.metadata();
                        // is_symlink() before is_dir(): some SFTP servers
                        // report symlink-to-directory entries with both
                        // bits set. If we recursed into one, we'd walk
                        // outside the subtree the user asked to delete
                        // (and possibly outside the connection's chroot).
                        // Treat any symlink as a leaf and unlink it.
                        if attrs.is_symlink() {
                            self.sftp
                                .remove_file(&child)
                                .await
                                .map_err(|err| map_sftp("remove", &child, err))?;
                        } else if attrs.is_dir() {
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
        let kind = if attrs.is_dir() {
            EntryKind::Directory
        } else if attrs.is_symlink() {
            EntryKind::Symlink
        } else if attrs.is_regular() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let name = remote_path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(remote_path)
            .to_string();
        Ok(Some(RemoteEntry {
            name,
            kind,
            size: attrs.size.unwrap_or(0),
            modified: attrs
                .mtime
                .and_then(|t| chrono::DateTime::from_timestamp(t as i64, 0)),
            mode: attrs.permissions,
        }))
    }

    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
        let remote = self
            .sftp
            .open_with_flags(remote_path, OpenFlags::READ)
            .await
            .map_err(|e| map_sftp("open", remote_path, e))?;
        let mut buf = Vec::new();
        remote.take(MAX_PREVIEW_BYTES + 1).read_to_end(&mut buf).await?;
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
mod tests {
    use super::{chunk_offsets, read_local_full};

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

    use russh::server::{Auth, Msg, Session as ServerSession};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{
        Attrs, Data, FileAttributes, Handle as SftpHandle, OpenFlags, Status, StatusCode, Version,
    };
    use tokio::sync::Mutex;

    use super::{HostKeyDecision, SftpTransport};
    use crate::session::{AuthMethod, Protocol, Session};
    use crate::transport::Transport;
    use crate::tui::event::AppEvent;

    /// Path -> file contents, shared by every SFTP channel on the connection.
    type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    /// Cap on bytes returned per read reply. Smaller than the transfer chunk so
    /// each chunk needs several reads, exercising the short-read handling.
    const SHORT_READ: usize = 60_000;

    // ---- SSH server side ------------------------------------------------

    struct SshServer {
        channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
        store: Store,
    }

    // Native `async fn` in trait as of russh 0.60 — see the client Handler.
    impl russh::server::Handler for SshServer {
        type Error = russh::Error;

        async fn auth_password(&mut self, _u: &str, _p: &str) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            self.channels.lock().await.insert(channel.id(), channel);
            Ok(true)
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
            Ok(SftpHandle { id, handle: filename })
        }

        async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
            Ok(ok_status(id))
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let store = self.store.lock().await;
            let content = store.get(&handle).ok_or(StatusCode::NoSuchFile)?;
            let off = offset as usize;
            if off >= content.len() {
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
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(size),
                    ..Default::default()
                },
            })
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.fstat(id, path).await
        }

        async fn rename(
            &mut self,
            id: u32,
            oldpath: String,
            newpath: String,
        ) -> Result<Status, Self::Error> {
            let mut store = self.store.lock().await;
            // SFTP v3 semantics: RENAME fails when the target exists. This
            // deliberately exercises the client's remove-then-rename
            // fallback for upload finalization.
            if store.contains_key(&newpath) {
                return Err(StatusCode::Failure);
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(russh::server::Config {
            keys: vec![
                russh::keys::PrivateKey::random(
                    &mut rand::rng(),
                    russh::keys::Algorithm::Ed25519,
                )
                .unwrap(),
            ],
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
        let mut fut = Box::pin(SftpTransport::connect(&session, Some("pw"), ev_tx));
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
            !store.lock().await.contains_key("/uploaded.bin.part"),
            "temporary .part must be renamed away after a successful upload"
        );

        // ---- upload over an existing remote file ----
        // The test server enforces SFTP v3 rename semantics (fails when the
        // target exists), so this exercises the remove-then-rename fallback
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
            !store.lock().await.contains_key("/uploaded.bin.part"),
            "temporary .part must be renamed away after an overwriting upload"
        );
        let _ = tokio::fs::remove_file(&ul_src).await;

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
}
