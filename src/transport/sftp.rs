//! SFTP transport built on russh + russh-sftp.
//!
//! NOTE: russh and russh-sftp evolve their APIs across minor versions. The
//! shape below targets `russh` 0.49.x and `russh-sftp` 2.x. If a `cargo build`
//! reports method-not-found errors here, check the exact constructor / method
//! names against the version actually pulled in by `Cargo.lock`. The trait
//! interface in `transport::Transport` is stable; only this file should need
//! tweaking.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use russh::client::{self, Handle, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{load_secret_key, ssh_key};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::{mpsc, oneshot};

use crate::error::{self, BlinkError, Result};
use crate::known_hosts::{self, KeyStatus};
use crate::session::{AuthMethod, Protocol, Session};
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
    /// `host:port` string used as the lookup key in the known-hosts file.
    host: String,
    /// Sends unknown-key info to the TUI so a confirmation modal can appear.
    event_tx: Option<mpsc::UnboundedSender<crate::tui::event::AppEvent>>,
    /// Receives the user's decision back from the TUI.
    decision_rx: Option<oneshot::Receiver<HostKeyDecision>>,
}

#[async_trait]
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

        match known_hosts::check(&self.host, &key_type, &key_b64) {
            Ok(KeyStatus::Trusted) => return Ok(true),
            Ok(KeyStatus::Changed { stored_key_type, .. }) => {
                tracing::warn!(
                    host = %self.host,
                    stored = %stored_key_type,
                    presented = %key_type,
                    "host key mismatch — rejecting connection",
                );
                // Send the changed-key event so the TUI can surface a clear
                // error message rather than a generic connect failure.
                let event = crate::tui::event::AppEvent::HostKeyChanged {
                    host: self.host.clone(),
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
                    host = %self.host,
                    "known_hosts read error — rejecting connection: {e}"
                );
                return Ok(false);
            }
            Ok(KeyStatus::Unknown) => {}
        }

        // Unknown key: send the details to the TUI and await the user's call.
        let (decision_tx, decision_rx) = oneshot::channel();
        self.decision_rx = Some(decision_rx);

        let event = crate::tui::event::AppEvent::HostKeyUnknown {
            host: self.host.clone(),
            key_type: key_type.clone(),
            key_b64: key_b64.clone(),
            fingerprint,
            decision_tx,
        };

        let decision = match self.event_tx.take() {
            Some(tx) => {
                if tx.send(event).is_err() {
                    return Ok(false);
                }
                // The TUI must respond within 60 seconds, otherwise reject
                // to avoid hanging the connection indefinitely.
                let decision = match self.decision_rx.take() {
                    Some(rx) => match tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        rx,
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
                    },
                    None => HostKeyDecision::Reject,
                };
                decision
            }
            None => return Ok(false),
        };

        match decision {
            HostKeyDecision::AcceptAndSave => {
                if let Err(e) = known_hosts::append(&self.host, &key_type, &key_b64) {
                    tracing::warn!("could not save host key: {e}");
                }
                Ok(true)
            }
            HostKeyDecision::AcceptOnce => Ok(true),
            HostKeyDecision::Reject => Ok(false),
        }
    }
}

pub struct SftpTransport {
    handle: Handle<KnownHostsHandler>,
    sftp: SftpSession,
    buf: Vec<u8>,
}

impl SftpTransport {
    pub async fn connect(
        session: &Session,
        password: Option<&str>,
        app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    ) -> Result<Self> {
        let config = Arc::new(client::Config::default());
        let addr = format!("{}:{}", session.host, session.port);
        let host_key = addr.clone();

        let handler = KnownHostsHandler {
            host: host_key,
            event_tx: Some(app_event_tx),
            decision_rx: None,
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
                let kp = PrivateKeyWithHashAlg::new(Arc::new(kp), None)
                    .map_err(|e| BlinkError::auth(format!("key algorithm: {e}")))?;
                handle
                    .authenticate_publickey(username, kp)
                    .await
                    .map_err(|e| BlinkError::auth(e.to_string()))?
            }
            AuthMethod::Agent => {
                #[cfg(target_os = "windows")]
                {
                    return Err(BlinkError::auth(
                        "ssh-agent auth is not supported on Windows yet",
                    ));
                }
                #[cfg(not(target_os = "windows"))]
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
                        let auth_result = handle
                            .authenticate_publickey_with(username, identity, &mut agent)
                            .await;
                        match auth_result {
                            Ok(true) => {
                                succeeded = true;
                                break;
                            }
                            Ok(false) => {}
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
            buf: vec![0u8; 64 * 1024],
        })
    }
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
            .map_err(|e| BlinkError::transport(format!("readdir {remote_path}: {e}")))?;

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
        // Resume support: if a partial file exists, skip already-downloaded
        // bytes so interrupted transfers can pick up where they left off.
        let offset = tokio::fs::metadata(local_path)
            .await
            .ok()
            .map(|m| m.len())
            .unwrap_or(0);

        let mut remote = self
            .sftp
            .open_with_flags(remote_path, OpenFlags::READ)
            .await
            .map_err(|e| BlinkError::transport(format!("open {remote_path}: {e}")))?;

        let total = self
            .sftp
            .metadata(remote_path)
            .await
            .ok()
            .and_then(|m| m.size)
            .unwrap_or(0);

        // If the file changed server-side since the partial download started,
        // start over — the existing bytes no longer match.
        let offset = if offset <= total { offset } else { 0 };

        if offset > 0 {
            remote
                .seek(SeekFrom::Start(offset))
                .await
                .map_err(|e| BlinkError::transport(format!("seek {remote_path}: {e}")))?;
        }

        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut local = if offset > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(local_path)
                .await?
        } else {
            tokio::fs::File::create(local_path).await?
        };

        let mut done: u64 = offset;
        loop {
            let n = remote.read(&mut self.buf).await?;
            if n == 0 {
                break;
            }
            local.write_all(&self.buf[..n]).await?;
            done += n as u64;
            if let Some(tx) = &progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: done,
                    bytes_total: total,
                });
            }
        }
        local.flush().await?;
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
        let mut remote = self
            .sftp
            .open_with_flags(
                remote_path,
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            )
            .await
            .map_err(|e| BlinkError::transport(format!("open {remote_path}: {e}")))?;

        let mut done: u64 = 0;
        loop {
            let n = local.read(&mut self.buf).await?;
            if n == 0 {
                break;
            }
            remote.write_all(&self.buf[..n]).await?;
            done += n as u64;
            if let Some(tx) = &progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: done,
                    bytes_total: total,
                });
            }
        }
        remote.flush().await?;
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.sftp
            .rename(from, to)
            .await
            .map_err(|e| BlinkError::transport(format!("rename {from} -> {to}: {e}")))
    }

    async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
        self.sftp
            .remove_file(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("remove {remote_path}: {e}")))
    }

    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
        if !recursive {
            return self
                .sftp
                .remove_dir(remote_path)
                .await
                .map_err(|e| BlinkError::transport(format!("rmdir {remote_path}: {e}")));
        }

        enum Op {
            Visit(String),
            Remove(String),
        }

        let mut stack = vec![Op::Visit(remote_path.to_string())];
        while let Some(op) = stack.pop() {
            match op {
                Op::Visit(path) => {
                    let entries = self.sftp.read_dir(&path).await.map_err(|e| {
                        BlinkError::transport(format!("readdir {path}: {e}"))
                    })?;

                    stack.push(Op::Remove(path.clone()));

                    let mut to_recurse: Vec<Op> = Vec::new();
                    for e in entries {
                        let name = e.file_name();
                        if name == "." || name == ".." {
                            continue;
                        }
                        let child = super::join_remote(&path, &name);
                        let attrs = e.metadata();
                        if attrs.is_dir() {
                            to_recurse.push(Op::Visit(child));
                        } else {
                            self.sftp.remove_file(&child).await.map_err(|err| {
                                BlinkError::transport(format!("remove {child}: {err}"))
                            })?;
                        }
                    }
                    for op in to_recurse.into_iter().rev() {
                        stack.push(op);
                    }
                }
                Op::Remove(path) => {
                    self.sftp.remove_dir(&path).await.map_err(|e| {
                        BlinkError::transport(format!("rmdir {path}: {e}"))
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
        self.sftp
            .create_dir(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("mkdir {remote_path}: {e}")))
    }

    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
        let attrs = match self.sftp.metadata(remote_path).await {
            Ok(a) => a,
            Err(_) => return Ok(None),
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
            .map_err(|e| BlinkError::transport(format!("open {remote_path}: {e}")))?;
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
