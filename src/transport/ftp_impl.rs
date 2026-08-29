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
                $crate::transport::ftp_impl::check_ftp_path("rnfr", from)?;
                $crate::transport::ftp_impl::check_ftp_path("rnto", to)?;
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
                $crate::transport::ftp_impl::check_ftp_path("dele", remote_path)?;
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

/// Refuse a remote path that would break out of one FTP command into another.
///
/// FTP commands are newline-terminated text — suppaftp builds them as
/// `format!("RETR {p}")` and appends `\r\n` with no escaping whatsoever — so a
/// path carrying CR or LF is not a path, it is a second command the server
/// will run under the user's credentials. On a shared server that is a
/// privilege escalation: another tenant names a file `x\r\nDELE //…`, and it
/// fires the moment the victim lists or downloads it. NUL goes with them
/// because no filesystem blink talks to accepts one, and a C-string server
/// would truncate the command there.
///
/// This lives at the FTP boundary rather than in
/// [`crate::transport::join_remote`] on purpose. SFTP's wire format is
/// length-prefixed, so a name containing a newline addresses exactly the file
/// it names and is safe to fetch; rejecting it there would cost SFTP users
/// access to legitimately-named files to fix a bug that is purely FTP's.
///
/// Every entry point that puts a path on the control channel calls this, so a
/// path is checked once, on the way in, rather than at each of the commands it
/// may fan out into.
pub(crate) fn check_ftp_path(op: &str, path: &str) -> Result<()> {
    if path.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0')) {
        // `BlinkError::transport` sanitizes, so the offending bytes render as
        // spaces rather than reaching the terminal on their way to the log.
        return Err(BlinkError::transport(format!(
            "{op}: refusing remote path containing a newline or null byte: {path}"
        )));
    }
    Ok(())
}

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
    check_ftp_path("list", remote_path)?;
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
    check_ftp_path("retr", remote_path)?;
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
    // Guards `part` too: it is `remote_path` plus a literal suffix.
    check_ftp_path("stor", remote_path)?;
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
    check_ftp_path("rmd", remote_path)?;
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
                    // See the SFTP path: an unjoinable name is skipped, not
                    // folded onto the directory being walked.
                    let Some(child) = crate::transport::join_remote(&path, name) else {
                        tracing::warn!(
                            dir = %path,
                            "skipping unusable entry name in recursive delete",
                        );
                        continue;
                    };
                    // The name came off this server's own listing, so the
                    // path built from it is no more trustworthy than the
                    // bytes were. Skip it rather than failing the whole
                    // delete: one hostile entry should not strand the rest.
                    if check_ftp_path("dele", &child).is_err() {
                        tracing::warn!(
                            dir = %path,
                            "skipping entry whose name would break the control channel",
                        );
                        continue;
                    }
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
    check_ftp_path("mkd", remote_path)?;
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
    // Guards `parent` too: it is a prefix of `remote_path`.
    check_ftp_path("metadata list", remote_path)?;
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
    check_ftp_path("retr", remote_path)?;
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::check_ftp_path;

    #[test]
    fn an_ordinary_path_is_allowed() {
        assert!(check_ftp_path("list", "/var/www/html").is_ok());
    }

    #[test]
    fn spaces_and_unicode_stay_allowed() {
        // The check is about the control channel's line framing, not about
        // what makes a tidy filename. Narrowing it further would start
        // refusing files people legitimately have.
        assert!(check_ftp_path("retr", "/srv/my report (final).pdf").is_ok());
        assert!(check_ftp_path("retr", "/srv/résumé — 2026.txt").is_ok());
    }

    #[test]
    fn crlf_is_refused() {
        // The whole point: `LIST /pub\r\nDELE /important.txt` is two commands.
        let err = check_ftp_path("list", "/pub\r\nDELE /important.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("newline"), "unhelpful message: {err}");
    }

    #[test]
    fn a_bare_lf_or_cr_is_refused_on_its_own() {
        // Servers disagree about which byte ends a command; refuse both
        // rather than betting on the peer being strict about CRLF.
        assert!(check_ftp_path("retr", "/srv/evil\nDELE /x").is_err());
        assert!(check_ftp_path("retr", "/srv/evil\rDELE /x").is_err());
    }

    #[test]
    fn a_nul_byte_is_refused() {
        assert!(check_ftp_path("retr", "/srv/evil\0truncated").is_err());
    }

    #[test]
    fn the_offending_bytes_do_not_reach_the_message_verbatim() {
        // The path is echoed back so the user can tell which entry was
        // refused, and it is the attacker's string — so it must arrive
        // sanitized, not raw, on its way to the log and the TUI.
        let err = check_ftp_path("list", "/pub\r\n\x1b[2JDELE /x")
            .unwrap_err()
            .to_string();
        assert!(!err.contains('\r'), "CR reached the message: {err:?}");
        assert!(!err.contains('\n'), "LF reached the message: {err:?}");
        assert!(!err.contains('\x1b'), "ESC reached the message: {err:?}");
    }
}

/// FTP integration tests against an in-process FTP server. No external
/// daemon is required. The server speaks only the commands blink issues and
/// serves data in short slices, so partial reads are exercised rather than
/// assumed away.
///
/// Written against suppaftp 8.0.5 deliberately: a harness written against a
/// new version cannot tell "encodes current behaviour" from "encodes the new
/// library's behaviour". Landed first, it is a differential test.
#[cfg(test)]
mod integration {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    use crate::session::{AuthMethod, Protocol, Session};
    use crate::transport::Transport;
    use crate::transport::ftp::FtpTransport;

    /// Absolute path -> file contents. A key ending in `/` is a directory.
    pub(super) type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    /// Every control-channel command the server received, verbatim and in
    /// order. Some behaviour is invisible from the result alone — a resume
    /// that silently restarts still lands the correct bytes — so tests assert
    /// on what was *issued*, not only on what came back.
    pub(super) type Log = Arc<Mutex<Vec<String>>>;

    /// Protocol-level malformations the server emits on demand. suppaftp 10.0
    /// converted these from panic to `FtpError`; no off-the-shelf server will
    /// produce them on request, which is why this one is hand-rolled.
    ///
    /// All three fields are read as of this commit: `bad_pasv_octet` and
    /// `unparsable_list_line` by PASV/LIST below, and `abrupt_close` by
    /// LIST's abrupt-close branch. The later RETR/STOR tasks that extend
    /// `handle_control` add their own reads of `abrupt_close`.
    #[derive(Clone, Default)]
    pub(super) struct Faults {
        /// PASV reply carrying an out-of-range octet.
        pub bad_pasv_octet: bool,
        /// A LIST body no parser can turn into entries.
        pub unparsable_list_line: bool,
        /// Close control and data connections after `150`, sending no `226`.
        pub abrupt_close: bool,
    }

    /// Bytes per data-connection write. Smaller than the transfer chunk so the
    /// client's read loop genuinely reassembles across reads.
    const DATA_SLICE: usize = 4096;

    /// Unix `ls -l` style listing of the immediate children of `dir`.
    /// suppaftp's parser expects this shape; a key ending in `/` is a
    /// directory and renders with a `d` mode prefix.
    fn listing_for(files: &HashMap<String, Vec<u8>>, dir: &str) -> String {
        let prefix = if dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}/")
        };

        let mut out = String::new();
        for (path, bytes) in files {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let trimmed = rest.trim_end_matches('/');
            // Immediate children only: no interior separator.
            if trimmed.is_empty() || trimmed.contains('/') {
                continue;
            }
            let (mode, size) = if path.ends_with('/') {
                ("drwxr-xr-x", 4096)
            } else {
                ("-rw-r--r--", bytes.len())
            };
            out.push_str(&format!(
                "{mode} 1 owner group {size:>12} Nov 01 12:00 {trimmed}\r\n"
            ));
        }
        out
    }

    fn test_session(port: u16) -> Session {
        Session {
            name: "it".to_string(),
            protocol: Protocol::Ftp,
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

    /// Binds :0, spawns the accept loop, returns the bound port and a count of
    /// accepted control connections. The dispatcher opens one per worker; the
    /// counter is how reuse is asserted, as in the SFTP harness.
    pub(super) async fn start_server(store: Store, faults: Faults) -> (u16, Arc<AtomicUsize>, Log) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connects = Arc::new(AtomicUsize::new(0));
        let connects_l = Arc::clone(&connects);
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let log_l = Arc::clone(&log);

        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                connects_l.fetch_add(1, Ordering::SeqCst);
                let store = Arc::clone(&store);
                let faults = faults.clone();
                let log = Arc::clone(&log_l);
                tokio::spawn(async move {
                    let _ = handle_control(sock, store, faults, log).await;
                });
            }
        });

        (port, connects, log)
    }

    /// One control connection. Extended by later tasks; unknown commands get
    /// `502` so a blink change that starts issuing a new command fails loudly
    /// instead of hanging.
    async fn handle_control(
        mut sock: TcpStream,
        store: Store,
        faults: Faults,
        log: Log,
    ) -> std::io::Result<()> {
        let (read_half, mut w) = sock.split();
        let mut lines = BufReader::new(read_half).lines();

        w.write_all(b"220 blink test server\r\n").await?;

        // Bound by PASV, consumed by the next data command.
        let mut pasv: Option<TcpListener> = None;
        // Set by REST, consumed by the next RETR, then cleared.
        let mut rest: u64 = 0;

        while let Some(line) = lines.next_line().await? {
            let line = line.trim_end();
            let (cmd, arg) = match line.split_once(' ') {
                Some((c, a)) => (c.to_ascii_uppercase(), a.to_string()),
                None => (line.to_ascii_uppercase(), String::new()),
            };
            // Every command verbatim, so a test can assert what was issued
            // rather than only what came back. Resume in particular is
            // invisible from the result alone: a download that silently
            // restarts still lands the correct bytes.
            log.lock().await.push(line.to_string());

            match cmd.as_str() {
                "USER" => w.write_all(b"331 password required\r\n").await?,
                "PASS" => w.write_all(b"230 logged in\r\n").await?,
                "TYPE" => w.write_all(b"200 type set\r\n").await?,
                "PASV" => {
                    let data_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
                    let port = data_listener.local_addr()?.port();
                    pasv = Some(data_listener);
                    let reply = if faults.bad_pasv_octet {
                        "227 Entering Passive Mode (127,0,0,1,999,0)\r\n".to_string()
                    } else {
                        format!(
                            "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                            port / 256,
                            port % 256
                        )
                    };
                    w.write_all(reply.as_bytes()).await?;
                }
                "LIST" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    let body = if faults.unparsable_list_line {
                        "!! this is not a listing line !!\r\n".to_string()
                    } else {
                        let files = store.lock().await;
                        let dir = if arg.is_empty() { "/" } else { arg.as_str() };
                        listing_for(&files, dir)
                    };
                    w.write_all(b"150 here comes the listing\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    data.write_all(body.as_bytes()).await?;
                    data.shutdown().await?;
                    drop(data);
                    w.write_all(b"226 transfer complete\r\n").await?;
                }
                "SIZE" => {
                    let files = store.lock().await;
                    match files.get(&arg) {
                        Some(bytes) => {
                            w.write_all(format!("213 {}\r\n", bytes.len()).as_bytes())
                                .await?
                        }
                        None => w.write_all(b"550 no such file\r\n").await?,
                    }
                }
                "RETR" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    let body = {
                        let files = store.lock().await;
                        match files.get(&arg) {
                            Some(bytes) => bytes.clone(),
                            None => {
                                w.write_all(b"550 no such file\r\n").await?;
                                continue;
                            }
                        }
                    };
                    let start = std::mem::take(&mut rest) as usize;
                    let slice = body.get(start..).unwrap_or(&[]).to_vec();

                    w.write_all(b"150 opening data connection\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    for chunk in slice.chunks(DATA_SLICE) {
                        data.write_all(chunk).await?;
                    }
                    data.shutdown().await?;
                    drop(data);
                    w.write_all(b"226 transfer complete\r\n").await?;
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

    #[tokio::test]
    async fn connecting_logs_in_and_sets_binary_mode() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, connects, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw"))
            .await
            .expect("connect and login should succeed");

        assert_eq!(transport.protocol(), Protocol::Ftp);
        assert_eq!(connects.load(Ordering::SeqCst), 1);

        transport.close().await.expect("QUIT should be clean");
    }

    #[tokio::test]
    async fn listing_a_directory_returns_its_entries() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/pub/one.txt".to_string(), b"hello".to_vec());
            files.insert("/pub/two.bin".to_string(), vec![0u8; 4096]);
            files.insert("/pub/sub/".to_string(), Vec::new());
            // Not an immediate child; must not appear.
            files.insert("/pub/sub/deep.txt".to_string(), b"x".to_vec());
        }
        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let mut entries = transport.list("/pub").await.expect("list should succeed");
        entries.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));

        let names: Vec<&str> = entries.iter().map(|e| e.raw_name.as_str()).collect();
        assert_eq!(names, vec!["one.txt", "sub", "two.bin"]);

        let one = &entries[0];
        assert_eq!(one.size, 5);
        assert_eq!(one.kind, crate::transport::EntryKind::File);

        let sub = &entries[1];
        assert_eq!(sub.kind, crate::transport::EntryKind::Directory);
    }

    /// Deterministic pseudo-random bytes (xorshift64), matching the SFTP
    /// harness's helper so payloads are reproducible across runs.
    fn pseudo_random(n: usize) -> Vec<u8> {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// A unique scratch directory for one test, removed by the OS on
    /// reboot. Tests that write files use this rather than the repo tree.
    /// Each caller passes its own distinct tag so concurrent tests never
    /// collide on the same directory.
    fn tempdir_for_test(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blink-ftp-it-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn downloading_preserves_every_byte() {
        let payload = pseudo_random(200_000);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/big.bin".to_string(), payload.clone());

        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let dir = tempdir_for_test("download");
        let local = dir.join("big.bin");
        transport
            .download("/big.bin", &local, None)
            .await
            .expect("download should succeed");

        let got = std::fs::read(&local).unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload);

        // Progress reporting depends on a real size, not a guess: the
        // download must consult SIZE before transfer.
        let issued = log.lock().await.clone();
        assert!(
            issued.iter().any(|c| c == "SIZE /big.bin"),
            "the download must consult SIZE for its progress total; commands issued: {issued:?}",
        );
    }

    /// `ftp_metadata` resolves size from a LIST of the parent directory, not
    /// from a SIZE command, so this exercises LIST's size field, not SIZE.
    #[tokio::test]
    async fn metadata_reports_the_size_the_server_gave() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/a.txt".to_string(), b"twelve bytes".to_vec());

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let meta = transport.metadata("/a.txt").await.unwrap().expect("present");
        assert_eq!(meta.size, 12);
    }
}
