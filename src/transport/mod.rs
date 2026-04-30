//! Connection abstraction.
//!
//! Adding a new protocol means:
//!   1. Implement [`Transport`] in a new file under `transport/`.
//!   2. Add a match arm in [`open`].
//!
//! Everything else in the app (TUI, transfer manager, session model) talks to
//! `Box<dyn Transport>`, not to a specific protocol.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::session::{Protocol, Session};

pub mod ftp;
pub mod ftps;
pub mod scp;
pub mod sftp;

/// One entry from a remote directory listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    /// POSIX mode bits where available, else None (e.g. FTP).
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl RemoteEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Directory)
    }
}

/// Progress update emitted while a single file is in flight.
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// What every protocol implementation must provide.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Human-readable label, e.g. `Protocol::Sftp`.
    fn protocol(&self) -> Protocol;

    /// List entries in `remote_path`. Implementations must NOT include `.` or `..`.
    async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>>;

    /// Download `remote_path` to `local_path`, sending progress to `progress`
    /// if a sender is provided.
    async fn download(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()>;

    /// Upload `local_path` to `remote_path`.
    async fn upload(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()>;

    /// Rename / move on the remote side.
    async fn rename(&mut self, from: &str, to: &str) -> Result<()>;

    /// Delete a single remote file.
    async fn delete_file(&mut self, remote_path: &str) -> Result<()>;

    /// Delete a remote directory.
    ///
    /// When `recursive` is `false`, the implementation issues a single
    /// `rmdir`-equivalent call; the operation fails on non-empty directories.
    /// When `recursive` is `true`, the implementation walks `remote_path`
    /// post-order and removes every descendant before removing the root.
    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()>;

    /// Create a remote directory. Implementations should treat "already
    /// exists" as a non-error since recursive uploads call this best-effort
    /// for every level of the tree.
    async fn mkdir(&mut self, remote_path: &str) -> Result<()>;

    /// Stat a single remote path. Returns `None` if the path doesn't exist.
    /// Used by recursive walks and overwrite checks.
    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>>;

    /// Read a remote file fully into memory. Used for previewing small text
    /// files and images.
    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes>;

    /// Cleanly close the connection.
    async fn close(&mut self) -> Result<()>;
}

/// Build the right transport for `session`. The password (if any) must be
/// resolved by the caller before this is invoked — we never store it on disk.
///
/// `app_event_tx` is forwarded to the SFTP/SCP handler for the host-key
/// confirmation flow. FTP/FTPS do not use host-key verification.
pub async fn open(
    session: &Session,
    password: Option<&str>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
) -> Result<Box<dyn Transport>> {
    match session.protocol {
        Protocol::Sftp => Ok(Box::new(
            sftp::SftpTransport::connect(session, password, app_event_tx).await?,
        )),
        Protocol::Scp => Ok(Box::new(
            scp::ScpTransport::connect(session, password, app_event_tx).await?,
        )),
        Protocol::Ftp => Ok(Box::new(ftp::FtpTransport::connect(session, password).await?)),
        Protocol::Ftps => Ok(Box::new(ftps::FtpsTransport::connect(session, password).await?)),
    }
}

/// Join a remote base path and a name, normalising the slash.
///
/// `name` must be a single path component (a filename from a directory
/// listing). Leading slashes are stripped to prevent a server-controlled name
/// like `"/etc/shadow"` from producing an absolute remote path via the `//`
/// resolution most servers apply. Names containing a `..` component are
/// rejected (returning `base` unchanged) to prevent upward traversal.
pub(crate) fn join_remote(base: &str, name: &str) -> String {
    let name = name.trim_start_matches('/');
    // Reject any dotdot component: "../secret" or "a/../b" both traverse up.
    if name.split('/').any(|c| c == "..") {
        return base.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Compute the parent of a remote path.
pub(crate) fn parent_remote(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => "/".to_string(),
    }
}

