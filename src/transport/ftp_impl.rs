//! Shared FTP/FTPS transport logic.
//!
//! Both `FtpTransport` and `FtpsTransport` wrap `ImplAsyncFtpStream<T>` with
//! different `T` parameters (`AsyncNoTlsStream` vs `AsyncRustlsStream`).
//! Since the generic struct provides identical methods regardless of `T`,
//! this module provides a macro that generates a full [`Transport`] impl
//! for any wrapper type that has a `stream: ImplAsyncFtpStream<T>` field.

macro_rules! delegate_ftp_transport {
    ($ty:ty, $proto_variant:ident) => {
        #[async_trait::async_trait]
        impl $crate::transport::Transport for $ty {
            fn protocol(&self) -> $crate::session::Protocol {
                $crate::session::Protocol::$proto_variant
            }

            async fn list(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<Vec<$crate::transport::RemoteEntry>> {
                $crate::transport::ftp_impl::ftp_list(&mut self.stream, remote_path).await
            }

            async fn download(
                &mut self,
                remote_path: &str,
                local_path: &std::path::Path,
                progress: Option<
                    tokio::sync::mpsc::UnboundedSender<$crate::transport::ProgressUpdate>,
                >,
            ) -> $crate::error::Result<()> {
                $crate::transport::ftp_impl::ftp_download(
                    &mut self.stream,
                    remote_path,
                    local_path,
                    progress,
                )
                .await
            }

            async fn upload(
                &mut self,
                local_path: &std::path::Path,
                remote_path: &str,
                progress: Option<
                    tokio::sync::mpsc::UnboundedSender<$crate::transport::ProgressUpdate>,
                >,
            ) -> $crate::error::Result<()> {
                $crate::transport::ftp_impl::ftp_upload(
                    &mut self.stream,
                    local_path,
                    remote_path,
                    progress,
                )
                .await
            }

            async fn rename(&mut self, from: &str, to: &str) -> $crate::error::Result<()> {
                let label = format!("{from} -> {to}");
                $crate::transport::ftp_impl::timed_ftp(
                    "rename",
                    &label,
                    self.stream.rename(from, to),
                )
                .await
            }

            async fn delete_file(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<()> {
                $crate::transport::ftp_impl::timed_ftp(
                    "dele",
                    remote_path,
                    self.stream.rm(remote_path),
                )
                .await
            }

            async fn delete_dir(
                &mut self,
                remote_path: &str,
                recursive: bool,
            ) -> $crate::error::Result<()> {
                $crate::transport::ftp_impl::ftp_delete_dir(
                    &mut self.stream,
                    remote_path,
                    recursive,
                )
                .await
            }

            async fn mkdir(&mut self, remote_path: &str) -> $crate::error::Result<()> {
                $crate::transport::ftp_impl::ftp_mkdir(&mut self.stream, remote_path).await
            }

            async fn metadata(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<Option<$crate::transport::RemoteEntry>> {
                $crate::transport::ftp_impl::ftp_metadata(&mut self.stream, remote_path).await
            }

            async fn read_to_bytes(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<bytes::Bytes> {
                $crate::transport::ftp_impl::ftp_read_to_bytes(
                    &mut self.stream,
                    remote_path,
                )
                .await
            }

            async fn close(&mut self) -> $crate::error::Result<()> {
                let _ = self.stream.quit().await;
                Ok(())
            }
        }
    };
}

pub(crate) use delegate_ftp_transport;

// ---------------------------------------------------------------------------
// Shared helper functions
// ---------------------------------------------------------------------------

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use bytes::Bytes;
use suppaftp::list::File as FtpFile;
use suppaftp::tokio::{ImplAsyncFtpStream, TokioTlsStream};
use suppaftp::FtpError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{BlinkError, Result};
use crate::transport::error_map::map_ftp;
use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry};

/// Cap on bytes read by `read_to_bytes`. See the equivalent in `sftp.rs`:
/// derived from the viewer's image limit so the two cannot drift apart.
pub(crate) const MAX_PREVIEW_BYTES: u64 = crate::preview::IMAGE_VIEW_LIMIT;

/// Per-operation timeout on the FTP / FTPS control channel.
///
/// suppaftp doesn't expose the underlying TCP socket, so we can't set
/// `SO_KEEPALIVE` the way the SFTP transport does — and an FTP control
/// channel that accepts a command but never responds will pin a worker
/// for whatever the OS keepalive interval is (often minutes). Wrap each
/// control-channel call in a 60 s deadline; a stalled server tears the
/// op down as [`BlinkError::Disconnected`] instead of holding the
/// transfer manager hostage.
///
/// Data-transfer loops (the read/write loops inside `ftp_download` and
/// `ftp_upload`) are *not* wrapped — those have natural progress
/// signals (bytes per second on the live transfer strip) and a stalled
/// data channel shows up as 0 MB/s. The user can cancel via `c`.
pub(crate) const FTP_OP_TIMEOUT: Duration = Duration::from_secs(60);

/// Run an FTP control-channel call with a deadline and classify the
/// result. On timeout: `Disconnected`. On underlying error:
/// [`map_ftp`]. Combines the timeout and the error-mapping that every
/// FTP call site previously had as separate `tokio::time::timeout` +
/// `.map_err(|e| map_ftp(...))` wrappers.
pub(crate) async fn timed_ftp<T, F>(op: &str, path: &str, fut: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, FtpError>>,
{
    match tokio::time::timeout(FTP_OP_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(map_ftp(op, path, e)),
        Err(_) => Err(BlinkError::disconnected(format!(
            "{op} {path}: no response in {}s",
            FTP_OP_TIMEOUT.as_secs(),
        ))),
    }
}

pub async fn ftp_list<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Vec<RemoteEntry>> {
    let lines = timed_ftp("list", remote_path, stream.list(Some(remote_path))).await?;

    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.starts_with("total ") {
            continue;
        }
        let parsed = match FtpFile::from_str(&line) {
            Ok(f) => f,
            Err(_) => continue,
        };
        // The server's own bytes — see `RemoteEntry::new`. Sanitizing here
        // would produce a name that no longer addresses the file.
        let raw_name = parsed.name().to_string();
        if raw_name == "." || raw_name == ".." {
            continue;
        }
        let kind = if parsed.is_directory() {
            EntryKind::Directory
        } else if parsed.is_symlink() {
            EntryKind::Symlink
        } else if parsed.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        out.push(RemoteEntry::new(
            raw_name,
            kind,
            parsed.size() as u64,
            None,
            None,
        ));
    }
    Ok(out)
}

pub async fn ftp_download<T: TokioTlsStream + Send + 'static>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    local_path: &Path,
    progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
) -> Result<()> {
    if let Some(parent) = local_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Stream into `<local>.part` and rename on success — see
    // [`crate::transport::part_path`] for the rationale.
    let part = super::part_path(local_path);

    // Wrap size() in a timeout too — a server that hangs on SIZE would
    // pin the whole download here. On timeout / error the size is simply
    // unknown; the progress bar just can't show a percentage.
    let reported_size = tokio::time::timeout(FTP_OP_TIMEOUT, stream.size(remote_path))
        .await
        .ok()
        .and_then(|r| r.ok())
        .map(|n| n as u64);
    let total = reported_size.unwrap_or(0);

    // Resume only a partial identifiable as this file — see
    // `transport::decide_resume`. The previous guard here compared lengths
    // and was written `total > 0 && offset > total`, so a server that
    // doesn't answer SIZE skipped the staleness check entirely and resumed
    // whatever happened to be on disk. Identity is checked independently of
    // size, so an unknown size no longer means an unchecked resume.
    let offset = super::resume_offset(local_path, remote_path, reported_size).await;

    if offset > 0 {
        timed_ftp("rest", remote_path, stream.resume_transfer(offset as usize)).await?;
    }

    let mut reader = timed_ftp("retr", remote_path, stream.retr_as_stream(remote_path)).await?;

    let mut local = if offset > 0 {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .await?
    } else {
        tokio::fs::File::create(&part).await?
    };
    // Identify the partial before writing to it — see the SFTP path.
    super::write_part_meta(local_path, remote_path, reported_size).await;

    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = offset;
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| BlinkError::transport(format!("read {remote_path}: {e}")))?;
        if n == 0 {
            break;
        }
        local
            .write_all(&buf[..n])
            .await
            .map_err(|e| BlinkError::transport(format!("write {}: {e}", part.display())))?;
        done += n as u64;
        if let Some(tx) = &progress {
            let _ = tx.send(ProgressUpdate {
                bytes_done: done,
                bytes_total: total,
            });
        }
    }
    local
        .flush()
        .await
        .map_err(|e| BlinkError::transport(format!("flush {}: {e}", part.display())))?;
    local
        .sync_all()
        .await
        .map_err(|e| BlinkError::transport(format!("sync {}: {e}", part.display())))?;
    drop(local);

    timed_ftp("finalize retr", remote_path, stream.finalize_retr_stream(reader)).await?;

    // Only rename once the server confirmed the transfer; otherwise a
    // truncated response could leave a corrupted "complete" file in place.
    tokio::fs::rename(&part, local_path)
        .await
        .map_err(|e| BlinkError::transport(format!("rename {}: {e}", local_path.display())))?;
    super::clear_part_meta(local_path).await;

    Ok(())
}

pub async fn ftp_upload<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    local_path: &Path,
    remote_path: &str,
    progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
) -> Result<()> {
    let total = tokio::fs::metadata(local_path).await?.len();
    let mut local = tokio::fs::File::open(local_path).await?;

    // Stream into `<remote>.part` and rename onto the final name only on
    // success, so an interrupted upload never leaves a truncated file under
    // the destination name. A failed upload may leave the `.part` behind:
    // after a data-channel error the control channel's state is uncertain,
    // so we don't risk further commands to clean it up.
    let part = format!("{remote_path}.part");

    let mut writer = timed_ftp("stor", &part, stream.put_with_stream(&part)).await?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = local.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .await
            .map_err(|e| BlinkError::transport(format!("write: {e}")))?;
        done += n as u64;
        if let Some(tx) = &progress {
            let _ = tx.send(ProgressUpdate {
                bytes_done: done,
                bytes_total: total,
            });
        }
    }
    writer
        .flush()
        .await
        .map_err(|e| BlinkError::transport(format!("flush: {e}")))?;
    timed_ftp("finalize put", &part, stream.finalize_put_stream(writer)).await?;

    // Move the fully-stored `.part` onto the final name. Whether RNTO
    // replaces an existing target is server-dependent: try the rename
    // first, and when it's refused, delete the target and retry once.
    if let Err(first_err) = timed_ftp("rename", remote_path, stream.rename(part.as_str(), remote_path)).await {
        // A dead control channel won't recover by retrying.
        if matches!(first_err, BlinkError::Disconnected(_)) {
            return Err(first_err);
        }
        match timed_ftp("dele", remote_path, stream.rm(remote_path)).await {
            Ok(()) => {}
            Err(BlinkError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        timed_ftp("rename", remote_path, stream.rename(part.as_str(), remote_path)).await?;
    }
    Ok(())
}

pub async fn ftp_delete_dir<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    recursive: bool,
) -> Result<()> {
    if !recursive {
        return timed_ftp("rmd", remote_path, stream.rmdir(remote_path)).await;
    }

    enum Op {
        Visit(String),
        Remove(String),
    }
    let mut stack = vec![Op::Visit(remote_path.to_string())];
    while let Some(op) = stack.pop() {
        match op {
            Op::Visit(path) => {
                let lines = timed_ftp("list", &path, stream.list(Some(&path))).await?;
                stack.push(Op::Remove(path.clone()));
                let mut subdirs: Vec<Op> = Vec::new();
                for line in lines {
                    if line.starts_with("total ") {
                        continue;
                    }
                    let parsed = match FtpFile::from_str(&line) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let name = parsed.name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let child = crate::transport::join_remote(&path, name);
                    if parsed.is_directory() {
                        subdirs.push(Op::Visit(child));
                    } else {
                        timed_ftp("dele", &child, stream.rm(&child)).await?;
                    }
                }
                for op in subdirs.into_iter().rev() {
                    stack.push(op);
                }
            }
            Op::Remove(path) => {
                timed_ftp("rmd", &path, stream.rmdir(&path)).await?;
            }
        }
    }
    Ok(())
}

pub async fn ftp_mkdir<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<()> {
    if let Ok(Some(existing)) = ftp_metadata(stream, remote_path).await {
        if existing.is_dir() {
            return Ok(());
        }
        return Err(BlinkError::transport(format!(
            "mkdir {remote_path}: path exists and is not a directory"
        )));
    }
    timed_ftp("mkd", remote_path, stream.mkdir(remote_path)).await
}

pub async fn ftp_metadata<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Option<RemoteEntry>> {
    let (parent, basename) = match remote_path.rsplit_once('/') {
        Some(("", b)) => ("/".to_string(), b.to_string()),
        Some((p, b)) => (p.to_string(), b.to_string()),
        None => (".".to_string(), remote_path.to_string()),
    };

    // Only treat NotFound as "file does not exist"; every other FtpError
    // (connection drop, secure-channel failure, unexpected response code)
    // is a real failure that needs to propagate. Without this, mid-walk
    // connection drops were being misreported as "the file disappeared".
    let lines = match timed_ftp("metadata list", &parent, stream.list(Some(&parent))).await {
        Ok(l) => l,
        Err(BlinkError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    for line in lines {
        if line.starts_with("total ") {
            continue;
        }
        let parsed = match FtpFile::from_str(&line) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if parsed.name() != basename {
            continue;
        }
        let kind = if parsed.is_directory() {
            EntryKind::Directory
        } else if parsed.is_symlink() {
            EntryKind::Symlink
        } else if parsed.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        return Ok(Some(RemoteEntry::new(
            basename,
            kind,
            parsed.size() as u64,
            None,
            None,
        )));
    }
    Ok(None)
}

pub async fn ftp_read_to_bytes<T: TokioTlsStream + Send + 'static>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Bytes> {
    let remote_path_owned = remote_path.to_string();
    let buf = timed_ftp(
        "retr",
        remote_path,
        stream.retr(&remote_path_owned, move |reader| {
            Box::pin(async move {
                let mut buf = Vec::new();
                let mut limited = reader.take(MAX_PREVIEW_BYTES + 1);
                limited
                    .read_to_end(&mut buf)
                    .await
                    .map_err(suppaftp::FtpError::ConnectionError)?;
                let reader = limited.into_inner();
                if buf.len() as u64 > MAX_PREVIEW_BYTES {
                    return Err(suppaftp::FtpError::ConnectionError(
                        std::io::Error::other("file exceeds preview size limit"),
                    ));
                }
                Ok((buf, reader))
            })
        }),
    )
    .await?;
    Ok(Bytes::from(buf))
}
