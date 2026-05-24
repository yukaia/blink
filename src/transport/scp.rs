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
//!
//! ## Why the macro
//!
//! Every method on `Transport` for `ScpTransport` is a one-liner that calls
//! the corresponding method on `self.inner` (an [`SftpTransport`]). Writing
//! that out method-for-method invites drift: someone adds a pre-condition
//! check to `SftpTransport::mkdir` and the SCP wrapper silently misses it,
//! or the two grow incompatible argument lists during a refactor without
//! the trait catching it. The [`delegate_inner_transport!`] macro forces
//! the body of each method to be exactly `self.inner.METHOD(args).await`
//! so there is no opportunity for the wrapper to introduce its own logic.

use async_trait::async_trait;

use crate::error::Result;
use crate::session::Session;
use crate::transport::sftp::SftpTransport;

/// Wraps an [`SftpTransport`] and reports its protocol as
/// [`crate::session::Protocol::Scp`]. Every other method delegates verbatim
/// via [`delegate_inner_transport!`].
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

/// Generate a [`Transport`](crate::transport::Transport) impl that forwards
/// every method to `self.inner.METHOD(...).await`.
///
/// `$ty` must have a field `inner` whose type itself implements
/// [`Transport`](crate::transport::Transport) (currently only
/// [`SftpTransport`]). `$proto_variant` is the
/// [`Protocol`](crate::session::Protocol) variant the wrapper should report
/// from `protocol()`.
///
/// Behavioural drift is impossible by construction: the macro never inspects
/// arguments, so any change to the inner type's behaviour propagates through
/// untouched.
macro_rules! delegate_inner_transport {
    ($ty:ty, $proto_variant:ident) => {
        #[async_trait]
        impl $crate::transport::Transport for $ty {
            fn protocol(&self) -> $crate::session::Protocol {
                $crate::session::Protocol::$proto_variant
            }

            async fn list(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<Vec<$crate::transport::RemoteEntry>> {
                self.inner.list(remote_path).await
            }

            async fn download(
                &mut self,
                remote_path: &str,
                local_path: &std::path::Path,
                progress: Option<
                    tokio::sync::mpsc::UnboundedSender<$crate::transport::ProgressUpdate>,
                >,
            ) -> $crate::error::Result<()> {
                self.inner.download(remote_path, local_path, progress).await
            }

            async fn upload(
                &mut self,
                local_path: &std::path::Path,
                remote_path: &str,
                progress: Option<
                    tokio::sync::mpsc::UnboundedSender<$crate::transport::ProgressUpdate>,
                >,
            ) -> $crate::error::Result<()> {
                self.inner.upload(local_path, remote_path, progress).await
            }

            async fn rename(&mut self, from: &str, to: &str) -> $crate::error::Result<()> {
                self.inner.rename(from, to).await
            }

            async fn delete_file(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<()> {
                self.inner.delete_file(remote_path).await
            }

            async fn delete_dir(
                &mut self,
                remote_path: &str,
                recursive: bool,
            ) -> $crate::error::Result<()> {
                self.inner.delete_dir(remote_path, recursive).await
            }

            async fn mkdir(&mut self, remote_path: &str) -> $crate::error::Result<()> {
                self.inner.mkdir(remote_path).await
            }

            async fn metadata(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<Option<$crate::transport::RemoteEntry>> {
                self.inner.metadata(remote_path).await
            }

            async fn read_to_bytes(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<bytes::Bytes> {
                self.inner.read_to_bytes(remote_path).await
            }

            async fn close(&mut self) -> $crate::error::Result<()> {
                self.inner.close().await
            }
        }
    };
}

delegate_inner_transport!(ScpTransport, Scp);
