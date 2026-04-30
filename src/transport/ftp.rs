//! FTP transport via `suppaftp::AsyncFtpStream` (tokio backend).
//!
//! Implementation notes:
//!
//! - The control connection holds the auth context. Worker tasks open their
//!   own connections, so we don't try to share a single FtpStream across
//!   threads — each [`Transport::open`] handshakes anew.
//! - Listings use the LIST command (Unix `ls -l` style output) and parse via
//!   `suppaftp::list::File`. We do NOT call `size()` / `mdtm()` per entry —
//!   that would multiply round-trips by the directory size.
//! - Anonymous login is the convention when the configured username is empty:
//!   we send `anonymous` / `anonymous@`. Most public FTP archives expect this.
//! - The stream is set to binary mode (TYPE I) at connect time so file sizes
//!   don't get mangled by ASCII translation.
//! - Recursive directory delete walks via repeated `list` calls and a stack
//!   of pending paths, the same shape as the SFTP recursive-delete path.

use std::path::Path;
use std::str::FromStr;

use async_trait::async_trait;
use bytes::Bytes;
use suppaftp::list::File as FtpFile;
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{self, BlinkError, Result};
use crate::session::{AuthMethod, Protocol, Session};
use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry, Transport};

/// Cap on bytes read by `read_to_bytes`. Matches the image preview limit so
/// a server that lies about a file's size in the directory listing cannot
/// cause unbounded allocation via `read_to_end`.
const MAX_PREVIEW_BYTES: u64 = 10_000_000; // 10 MB

pub struct FtpTransport {
    stream: AsyncFtpStream,
}

impl FtpTransport {
    pub async fn connect(session: &Session, password: Option<&str>) -> Result<Self> {
        if !matches!(session.auth, AuthMethod::Password) {
            return Err(BlinkError::auth(
                "FTP only supports password (or anonymous) auth",
            ));
        }

        let addr = format!("{}:{}", session.host, session.port);
        let mut stream = AsyncFtpStream::connect(&addr).await.map_err(|e| {
            BlinkError::connect(format!("ftp connect to {addr}: {e}"))
        })?;

        // Empty username -> anonymous. RFC-recommended password is the
        // user's email; "anonymous@" is the conventional placeholder when
        // we don't know it.
        let (user, pw) = if session.username.is_empty() {
            ("anonymous", "anonymous@")
        } else {
            let pw = password.unwrap_or("");
            (session.username.as_str(), pw)
        };
        stream
            .login(user, pw)
            .await
            .map_err(|e| BlinkError::auth(format!("ftp login: {e}")))?;

        // Binary mode — required for any non-text file to round-trip cleanly.
        stream
            .transfer_type(FileType::Binary)
            .await
            .map_err(|e| BlinkError::transport(format!("set binary: {e}")))?;

        Ok(Self { stream })
    }
}

#[async_trait]
impl Transport for FtpTransport {
    fn protocol(&self) -> Protocol {
        Protocol::Ftp
    }

    async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>> {
        let lines = self
            .stream
            .list(Some(remote_path))
            .await
            .map_err(|e| BlinkError::transport(format!("list {remote_path}: {e}")))?;

        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            // Some servers prepend a "total NN" header line — skip it.
            if line.starts_with("total ") {
                continue;
            }
            let parsed = match FtpFile::from_str(&line) {
                Ok(f) => f,
                Err(_) => continue, // unparseable line; skip rather than fail listing
            };
            // Sanitize before storing: entry names are server-controlled and
            // flow to the TUI pane via Span::raw(), which does not strip
            // control characters.
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
                // suppaftp's list parser exposes modified() as chrono::NaiveDate,
                // which doesn't carry a time component; treat as not-set rather
                // than fabricate a midnight UTC value.
                modified: None,
                mode: None,
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
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let local_path = local_path.to_path_buf();
        let progress_clone = progress.clone();

        // Try SIZE first so progress can show a percentage. If the server
        // doesn't support SIZE for this file, fall through with total=0 and
        // we'll just show a count.
        let total = self.stream.size(remote_path).await.unwrap_or(0) as u64;

        let remote_path_owned = remote_path.to_string();
        // The retr closure takes ownership of the data stream and must return
        // it back to the library so it can finalize the transfer. We do all
        // the actual file I/O inside the closure.
        self.stream
            .retr(&remote_path_owned, move |mut reader| {
                let local_path = local_path.clone();
                let progress = progress_clone.clone();
                Box::pin(async move {
                    let mut file = tokio::fs::File::create(&local_path)
                        .await
                        .map_err(|e| {
                            suppaftp::FtpError::ConnectionError(std::io::Error::other(
                                format!("create {}: {e}", local_path.display()),
                            ))
                        })?;
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut done: u64 = 0;
                    loop {
                        let n = reader.read(&mut buf).await.map_err(
                            suppaftp::FtpError::ConnectionError,
                        )?;
                        if n == 0 {
                            break;
                        }
                        file.write_all(&buf[..n]).await.map_err(
                            suppaftp::FtpError::ConnectionError,
                        )?;
                        done += n as u64;
                        if let Some(tx) = &progress {
                            let _ = tx.send(ProgressUpdate {
                                bytes_done: done,
                                bytes_total: total,
                            });
                        }
                    }
                    file.flush()
                        .await
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    Ok(((), reader))
                })
            })
            .await
            .map_err(|e| BlinkError::transport(format!("retr {remote_path}: {e}")))?;
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

        let mut writer = self
            .stream
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
        // CRITICAL: must call finalize_put_stream so the library reads the
        // 226 transfer-complete response off the control channel; otherwise
        // the next command sees stale response data.
        self.stream
            .finalize_put_stream(writer)
            .await
            .map_err(|e| BlinkError::transport(format!("finalize put: {e}")))?;
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.stream
            .rename(from, to)
            .await
            .map_err(|e| BlinkError::transport(format!("rename {from} -> {to}: {e}")))
    }

    async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
        self.stream
            .rm(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("dele {remote_path}: {e}")))
    }

    async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
        if !recursive {
            return self.stream.rmdir(remote_path).await.map_err(|e| {
                BlinkError::transport(format!("rmd {remote_path}: {e}"))
            });
        }

        // Iterative post-order walk, same shape as the SFTP version. The
        // FTP API doesn't expose a "stat one path" call cleanly, so we
        // detect dir-vs-file via the parent listing.
        enum Op {
            Visit(String),
            Remove(String),
        }
        let mut stack = vec![Op::Visit(remote_path.to_string())];
        while let Some(op) = stack.pop() {
            match op {
                Op::Visit(path) => {
                    let lines = self.stream.list(Some(&path)).await.map_err(|e| {
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
                        let child = super::join_remote(&path, name);
                        if parsed.is_directory() {
                            subdirs.push(Op::Visit(child));
                        } else {
                            self.stream.rm(&child).await.map_err(|e| {
                                BlinkError::transport(format!("dele {child}: {e}"))
                            })?;
                        }
                    }
                    for op in subdirs.into_iter().rev() {
                        stack.push(op);
                    }
                }
                Op::Remove(path) => {
                    self.stream.rmdir(&path).await.map_err(|e| {
                        BlinkError::transport(format!("rmd {path}: {e}"))
                    })?;
                }
            }
        }
        Ok(())
    }

    async fn mkdir(&mut self, remote_path: &str) -> Result<()> {
        // Best-effort: if it already exists, that's not a failure for our
        // recursive-upload caller, who calls mkdir for every level of the
        // destination tree. The FTP error type doesn't expose a structured
        // "exists" reply code through suppaftp's API in 8.x, so the test
        // here is a metadata probe.
        if let Ok(Some(existing)) = self.metadata(remote_path).await {
            if existing.is_dir() {
                return Ok(());
            }
            return Err(BlinkError::transport(format!(
                "mkdir {remote_path}: path exists and is not a directory"
            )));
        }
        self.stream
            .mkdir(remote_path)
            .await
            .map_err(|e| BlinkError::transport(format!("mkd {remote_path}: {e}")))
    }

    async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
        // FTP has no single-call stat. Strategy: list the parent directory
        // and find an entry whose name matches. For a path with no parent
        // (the root, or a bare name), list the current dir.
        let (parent, basename) = match remote_path.rsplit_once('/') {
            Some(("", b)) => ("/".to_string(), b.to_string()),
            Some((p, b)) => (p.to_string(), b.to_string()),
            None => (".".to_string(), remote_path.to_string()),
        };

        let lines = match self.stream.list(Some(&parent)).await {
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

    async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
        // suppaftp 8's async streams don't expose retr_as_buffer; do the
        // equivalent by hand inside the retr closure.
        //
        // Cap reads at MAX_PREVIEW_BYTES: the caller gates on the server-
        // reported file size, but a server can lie in its directory listing
        // and then stream unlimited data, bypassing that check.
        let remote_path_owned = remote_path.to_string();
        let buf = self
            .stream
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

    async fn close(&mut self) -> Result<()> {
        let _ = self.stream.quit().await;
        Ok(())
    }
}
