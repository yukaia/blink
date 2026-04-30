//! SCP transport — implemented as transparent SFTP.
//!
//! ## Why this isn't a "real" SCP implementation
//!
//! The original SCP wire protocol predates SSH itself; it works by `exec`ing
//! the remote `scp` binary in either source (`-f`) or sink (`-t`) mode and
//! ping-ponging a tiny line-based protocol over the SSH channel. It has
//! exactly two operations: send a file, receive a file. There's no listing,
//! rename, or delete. Implementing those for our [`Transport`] trait would
//! mean piggy-backing on side-channel `exec ls -la` and `exec rm` invocations,
//! which is brittle, locale-dependent, and a security smell.
//!
//! In practice, "scp the protocol" was deprecated in OpenSSH 9.0 (April 2022),
//! which made `scp(1)` use SFTP internally. Connecting `scp://` to a modern
//! server is already SFTP under the hood. So we do the same thing: when the
//! user picks `scp://`, we open an SFTP session and route every operation
//! through it.
//!
//! The user-visible difference from picking `sftp://` directly: none. The
//! only servers where this would matter are SCP-only legacy boxes (some
//! embedded systems, ancient routers); for those, a future revision could
//! add a real wire-protocol implementation gated on a session option, and
//! it would slot in here without touching anything else.

use std::path::Path;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::session::{Protocol, Session};
use crate::transport::sftp::SftpTransport;
use crate::transport::{ProgressUpdate, RemoteEntry, Transport};

/// Wraps an [`SftpTransport`] and reports its protocol as [`Protocol::Scp`].
/// Every other method delegates verbatim.
pub struct ScpTransport {
    inner: SftpTransport,
}

impl ScpTransport {
    pub async fn connect(
        session: &Session,
        password: Option<&str>,
        app_event_tx: tokio::sync::mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    ) -> Result<Self> {
        let inner = SftpTransport::connect(session, password, app_event_tx).await?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl Transport for ScpTransport {
    fn protocol(&self) -> Protocol {
        Protocol::Scp
    }

    async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>> {
        self.inner.list(remote_path).await
    }

    async fn download(
        &mut self,
        remote_path: &str,
        local_path: &Path,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()> {
        self.inner.download(remote_path, local_path, progress).await
    }

    async fn upload(
        &mut self,
        local_path: &Path,
        remote_path: &str,
        progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
    ) -> Result<()> {
        self.inner.upload(local_path, remote_path, progress).await
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to).await
    }

    async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
        self.inner.delete_file(remote_path).await
    }

    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
        self.inner.delete_dir(remote_path, recursive).await
    }

    async fn mkdir(&mut self, remote_path: &str) -> Result<()> {
        self.inner.mkdir(remote_path).await
    }

    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
        self.inner.metadata(remote_path).await
    }

    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
        self.inner.read_to_bytes(remote_path).await
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }
}
