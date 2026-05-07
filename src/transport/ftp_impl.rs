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
                self.stream
                    .rename(from, to)
                    .await
                    .map_err(|e| {
                        $crate::error::BlinkError::transport(format!(
                            "rename {from} -> {to}: {e}"
                        ))
                    })
            }

            async fn delete_file(
                &mut self,
                remote_path: &str,
            ) -> $crate::error::Result<()> {
                self.stream
                    .rm(remote_path)
                    .await
                    .map_err(|e| {
                        $crate::error::BlinkError::transport(format!(
                            "dele {remote_path}: {e}"
                        ))
                    })
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

use bytes::Bytes;
use suppaftp::list::File as FtpFile;
use suppaftp::tokio::{ImplAsyncFtpStream, TokioTlsStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{self, BlinkError, Result};
use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry};

const MAX_PREVIEW_BYTES: u64 = 10_000_000;

pub async fn ftp_list<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Vec<RemoteEntry>> {
    let lines = stream
        .list(Some(remote_path))
        .await
        .map_err(|e| BlinkError::transport(format!("list {remote_path}: {e}")))?;

    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.starts_with("total ") {
            continue;
        }
        let parsed = match FtpFile::from_str(&line) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let name = error::sanitize(parsed.name().to_string());
        if name == "." || name == ".." {
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
        out.push(RemoteEntry {
            name,
            kind,
            size: parsed.size() as u64,
            modified: None,
            mode: None,
        });
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

    // Resume support: if a partial file exists seek to its end so interrupted
    // transfers pick up where they left off (FTP REST command).
    let offset = tokio::fs::metadata(local_path)
        .await
        .ok()
        .map(|m| m.len())
        .unwrap_or(0);

    let total = stream.size(remote_path).await.unwrap_or(0) as u64;

    // If the partial file is larger than the server file, it's stale — restart.
    let offset = if total > 0 && offset > total {
        tracing::warn!(
            remote = %remote_path,
            local_bytes = offset,
            server_bytes = total,
            "FTP partial file is larger than server file — restarting download",
        );
        0
    } else {
        offset
    };

    if offset > 0 {
        stream
            .resume_transfer(offset as usize)
            .await
            .map_err(|e| BlinkError::transport(format!("rest {remote_path}: {e}")))?;
    }

    let mut reader = stream
        .retr_as_stream(remote_path)
        .await
        .map_err(|e| BlinkError::transport(format!("retr {remote_path}: {e}")))?;

    let mut local = if offset > 0 {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(local_path)
            .await?
    } else {
        tokio::fs::File::create(local_path).await?
    };

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
            .map_err(|e| BlinkError::transport(format!("write {}: {e}", local_path.display())))?;
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
        .map_err(|e| BlinkError::transport(format!("flush {}: {e}", local_path.display())))?;

    stream
        .finalize_retr_stream(reader)
        .await
        .map_err(|e| BlinkError::transport(format!("finalize retr {remote_path}: {e}")))?;

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

    let mut writer = stream
        .put_with_stream(remote_path)
        .await
        .map_err(|e| BlinkError::transport(format!("stor {remote_path}: {e}")))?;

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
    stream
        .finalize_put_stream(writer)
        .await
        .map_err(|e| BlinkError::transport(format!("finalize put: {e}")))?;
    Ok(())
}

pub async fn ftp_delete_dir<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    recursive: bool,
) -> Result<()> {
    if !recursive {
        return stream
            .rmdir(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("rmd {remote_path}: {e}")));
    }

    enum Op {
        Visit(String),
        Remove(String),
    }
    let mut stack = vec![Op::Visit(remote_path.to_string())];
    while let Some(op) = stack.pop() {
        match op {
            Op::Visit(path) => {
                let lines = stream.list(Some(&path)).await.map_err(|e| {
                    BlinkError::transport(format!("list {path}: {e}"))
                })?;
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
                        stream.rm(&child).await.map_err(|e| {
                            BlinkError::transport(format!("dele {child}: {e}"))
                        })?;
                    }
                }
                for op in subdirs.into_iter().rev() {
                    stack.push(op);
                }
            }
            Op::Remove(path) => {
                stream
                    .rmdir(&path)
                    .await
                    .map_err(|e| BlinkError::transport(format!("rmd {path}: {e}")))?;
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
    stream
        .mkdir(remote_path)
        .await
        .map_err(|e| BlinkError::transport(format!("mkd {remote_path}: {e}")))
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

    let lines = match stream.list(Some(&parent)).await {
        Ok(l) => l,
        Err(_) => return Ok(None),
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
        return Ok(Some(RemoteEntry {
            name: basename,
            kind,
            size: parsed.size() as u64,
            modified: None,
            mode: None,
        }));
    }
    Ok(None)
}

pub async fn ftp_read_to_bytes<T: TokioTlsStream + Send + 'static>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Bytes> {
    let remote_path_owned = remote_path.to_string();
    let buf = stream
        .retr(&remote_path_owned, move |reader| {
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
        })
        .await
        .map_err(|e| BlinkError::transport(format!("retr {remote_path}: {e}")))?;
    Ok(Bytes::from(buf))
}
