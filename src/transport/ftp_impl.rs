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

            async fn delete_file(&mut self, remote_path: &str) -> $crate::error::Result<()> {
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
                $crate::transport::ftp_impl::ftp_read_to_bytes(&mut self.stream, remote_path).await
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use suppaftp::FtpError;
use suppaftp::list::ListParser;
use suppaftp::tokio::{ImplAsyncFtpStream, TokioTlsStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::{BlinkError, Result};
use crate::transfer::MAX_QUEUED_JOBS;
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
///
/// suppaftp 10.0.2 also rejects CR/LF at the library boundary, so this is no
/// longer the only guard. It stays the primary one: it names the operation
/// and produces a sanitized `BlinkError::transport`, where suppaftp's would
/// arrive as an opaque `FtpError`. Do not remove it as redundant — and note
/// upstream's `validate_command_line` covers CR/LF only, so the NUL check
/// below has no backstop at all.
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
    let mut considered = 0usize;
    let mut skipped = 0usize;
    for line in lines {
        if line.starts_with("total ") {
            continue;
        }
        considered += 1;
        // blink issues LIST, never MLSD or MLST, so only the LIST parsers
        // are the right ones for this input. `File::from_str` would fall
        // through to the MLSX parsers, which split on `;`, ignore unknown
        // facts and name the file the last token — so any non-empty line
        // parses as a file named after itself.
        let parsed = match ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line))
        {
            Ok(f) => f,
            Err(_) => {
                skipped += 1;
                continue;
            }
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
    // One line per call, not one per skipped line: against a server whose
    // LIST format neither parser accepts, *every* line is unparsable, and
    // this runs on every interactive navigation — a per-line warn would
    // bury the log under one keystroke. Silence was worse still: the pane
    // just came up empty with nothing anywhere saying why.
    if skipped > 0 {
        tracing::warn!(
            path = %remote_path,
            skipped,
            considered,
            "skipped unparsable listing lines",
        );
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

    timed_ftp(
        "finalize retr",
        remote_path,
        stream.finalize_retr_stream(reader),
    )
    .await?;

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
    if let Err(first_err) = timed_ftp(
        "rename",
        remote_path,
        stream.rename(part.as_str(), remote_path),
    )
    .await
    {
        // A dead control channel won't recover by retrying.
        if matches!(first_err, BlinkError::Disconnected(_)) {
            return Err(first_err);
        }
        match timed_ftp("dele", remote_path, stream.rm(remote_path)).await {
            Ok(()) => {}
            Err(BlinkError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        timed_ftp(
            "rename",
            remote_path,
            stream.rename(part.as_str(), remote_path),
        )
        .await?;
    }
    Ok(())
}

pub async fn ftp_delete_dir<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    recursive: bool,
) -> Result<()> {
    ftp_delete_dir_capped(stream, remote_path, recursive, MAX_QUEUED_JOBS).await
}

/// `ftp_delete_dir` with the pending-work ceiling spelled out, so the guard
/// can be exercised without standing up a tree of `MAX_QUEUED_JOBS` entries.
pub async fn ftp_delete_dir_capped<T: TokioTlsStream + Send>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
    recursive: bool,
    limit: usize,
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
        // Same ceiling, and the same reason, as `walk_remote`: a server is
        // free to serve a tree deeper or wider than this process can hold,
        // and the stack is what grows with it. Checked after the pop so the
        // count is the work still outstanding, matching the walk.
        if stack.len() > limit {
            return Err(BlinkError::transport(format!(
                "recursive delete exceeded {limit} entries — \
                 narrow the target or remove it in smaller parts",
            )));
        }
        match op {
            Op::Visit(path) => {
                let lines = timed_ftp("list", &path, stream.list(Some(&path))).await?;
                stack.push(Op::Remove(path.clone()));
                let mut subdirs: Vec<Op> = Vec::new();
                for line in lines {
                    if line.starts_with("total ") {
                        continue;
                    }
                    let parsed = match ListParser::parse_posix(&line)
                        .or_else(|_| ListParser::parse_dos(&line))
                    {
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
    let mut found = None;
    let mut considered = 0usize;
    let mut skipped = 0usize;
    for line in lines {
        if line.starts_with("total ") {
            continue;
        }
        considered += 1;
        let parsed = match ListParser::parse_posix(&line).or_else(|_| ListParser::parse_dos(&line))
        {
            Ok(f) => f,
            Err(_) => {
                skipped += 1;
                continue;
            }
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
        found = Some(RemoteEntry::new(
            basename.clone(),
            kind,
            parsed.size() as u64,
            None,
            None,
        ));
        break;
    }
    // Aggregated for the same reason as `ftp_list`, and reported against
    // `remote_path` rather than the parent that was actually listed: the
    // path the caller asked about is the one they can act on. A stat that
    // answers "absent" only because the parser rejected every line is
    // otherwise indistinguishable from a genuinely missing file.
    if skipped > 0 {
        tracing::warn!(
            path = %remote_path,
            skipped,
            considered,
            "skipped unparsable listing lines",
        );
    }
    Ok(found)
}

pub async fn ftp_read_to_bytes<T: TokioTlsStream + Send + 'static>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Bytes> {
    check_ftp_path("retr", remote_path)?;
    let remote_path_owned = remote_path.to_string();
    // The cap is signalled out of band rather than as the callback's error.
    // An `Err` from the callback makes `retr` return without finalising the
    // transfer, which left the connection refusing every later data command
    // on suppaftp 10; and any `FtpError` it could carry maps to the wrong
    // thing — `ConnectionError` reads as a disconnect. So the callback always
    // returns `Ok`, `retr` finalises, and the flag decides the outcome.
    let over_cap = Arc::new(AtomicBool::new(false));
    let result = timed_ftp(
        "retr",
        remote_path,
        stream.retr(&remote_path_owned, {
            let over_cap = Arc::clone(&over_cap);
            move |reader| {
                // Cloned per call: `retr` takes an `FnMut`, so the closure
                // cannot give its own handle away to the future.
                let over_cap = Arc::clone(&over_cap);
                Box::pin(async move {
                    let mut buf = Vec::new();
                    let mut limited = reader.take(MAX_PREVIEW_BYTES + 1);
                    limited
                        .read_to_end(&mut buf)
                        .await
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    if buf.len() as u64 > MAX_PREVIEW_BYTES {
                        over_cap.store(true, Ordering::Relaxed);
                    }
                    Ok((buf, limited.into_inner()))
                })
            }
        }),
    )
    .await;
    // Checked before `result`: having stopped reading early, the server
    // usually answers 426, and that is not the error that happened.
    if over_cap.load(Ordering::Relaxed) {
        return Err(BlinkError::transport(format!(
            "retr {remote_path}: file exceeds preview size limit"
        )));
    }
    Ok(Bytes::from(result?))
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

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    /// Every field is read: `bad_pasv_octet` and `unparsable_list_line` by
    /// PASV/LIST below, `abrupt_close` by the abrupt-close branch of LIST,
    /// RETR and STOR/APPE alike, and `hostile_listing_names` by LIST.
    #[derive(Clone, Default)]
    pub(super) struct Faults {
        /// PASV reply carrying an out-of-range octet.
        pub bad_pasv_octet: bool,
        /// A LIST body no parser can turn into entries.
        pub unparsable_list_line: bool,
        /// Close control and data connections after `150`, sending no `226`.
        pub abrupt_close: bool,
        /// Extra LIST lines carrying names a walk must refuse to act on:
        /// `.` and `..`, a name that joins to nothing, and one holding a NUL.
        /// These cannot be injected through the store, because `listing_for`
        /// derives names from keys and drops any containing a separator.
        pub hostile_listing_names: bool,
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
    /// accepted control connections. Only the connect test reads the counter,
    /// to assert a single connection; FTP has no equivalent of the SFTP
    /// harness's pool-reuse test, so nothing here asserts reuse.
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
        // Set by RNFR, consumed by the next RNTO.
        let mut rename_from = String::new();

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
                    let mut body = if faults.unparsable_list_line {
                        "!! this is not a listing line !!\r\n".to_string()
                    } else {
                        let files = store.lock().await;
                        let dir = if arg.is_empty() { "/" } else { arg.as_str() };
                        listing_for(&files, dir)
                    };
                    if faults.hostile_listing_names {
                        // Regular files, so a walk that fails to skip one
                        // reaches DELE rather than recursing into it.
                        for name in ["..", ".", "/", "bad\0name.txt"] {
                            body.push_str(&format!(
                                "-rw-r--r-- 1 owner group {:>12} Nov 01 12:00 {name}\r\n",
                                0,
                            ));
                        }
                    }
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
                "REST" => {
                    rest = arg.trim().parse().unwrap_or(0);
                    w.write_all(b"350 restart position accepted\r\n").await?;
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
                    // Consumed unconditionally, even if this RETR fails
                    // below: a real server clears the restart marker on the
                    // next transfer command regardless of outcome, so a
                    // failed RETR must not leave a stale offset for the one
                    // after it.
                    let start = std::mem::take(&mut rest) as usize;
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
                    let slice = body.get(start..).unwrap_or(&[]).to_vec();

                    w.write_all(b"150 opening data connection\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    // A client may close the data connection before the body
                    // is sent — a preview stops reading at its size cap. A
                    // real daemon answers 426 and keeps serving; letting the
                    // write error escape would end the whole control
                    // connection instead, and whether it did would depend on
                    // how much of the body fit in the socket buffer.
                    let sent: std::io::Result<()> = async {
                        for chunk in slice.chunks(DATA_SLICE) {
                            data.write_all(chunk).await?;
                        }
                        data.shutdown().await
                    }
                    .await;
                    drop(data);
                    if sent.is_err() {
                        w.write_all(b"426 transfer aborted\r\n").await?;
                        continue;
                    }
                    w.write_all(b"226 transfer complete\r\n").await?;
                }
                "STOR" | "APPE" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    w.write_all(b"150 ready for data\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    let mut buf = Vec::new();
                    data.read_to_end(&mut buf).await?;
                    drop(data);
                    {
                        let mut files = store.lock().await;
                        if cmd == "APPE" {
                            files
                                .entry(arg.clone())
                                .or_default()
                                .extend_from_slice(&buf);
                        } else {
                            files.insert(arg.clone(), buf);
                        }
                    }
                    w.write_all(b"226 transfer complete\r\n").await?;
                }
                "RNFR" => {
                    rename_from = arg.clone();
                    w.write_all(b"350 ready for RNTO\r\n").await?;
                }
                "RNTO" => {
                    let mut files = store.lock().await;
                    match files.remove(&rename_from) {
                        Some(bytes) => {
                            files.insert(arg.clone(), bytes);
                            drop(files);
                            w.write_all(b"250 renamed\r\n").await?;
                        }
                        None => {
                            drop(files);
                            w.write_all(b"550 no such file\r\n").await?;
                        }
                    }
                }
                // A directory is a key with a trailing slash, so both arms
                // normalise the argument the client sent.
                "MKD" => {
                    let key = format!("{}/", arg.trim_end_matches('/'));
                    store.lock().await.insert(key, Vec::new());
                    w.write_all(b"257 directory created\r\n").await?;
                }
                "RMD" => {
                    let key = format!("{}/", arg.trim_end_matches('/'));
                    let removed = store.lock().await.remove(&key).is_some();
                    if removed {
                        w.write_all(b"250 directory removed\r\n").await?;
                    } else {
                        w.write_all(b"550 no such directory\r\n").await?;
                    }
                }
                "DELE" => {
                    let removed = store.lock().await.remove(&arg).is_some();
                    if removed {
                        w.write_all(b"250 file deleted\r\n").await?;
                    } else {
                        w.write_all(b"550 no such file\r\n").await?;
                    }
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
        let (port, connects, log) = start_server(Arc::clone(&store), Faults::default()).await;

        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw"))
            .await
            .expect("connect and login should succeed");

        assert_eq!(transport.protocol(), Protocol::Ftp);
        assert_eq!(connects.load(Ordering::SeqCst), 1);

        transport
            .close()
            .await
            .expect("close should report success");

        // `close` discards `quit`'s result and returns `Ok(())`, so the call
        // above asserts nothing on its own — only the log shows QUIT went out.
        // TYPE I matters more: FTP defaults to ASCII, and a server doing CRLF
        // translation corrupts every binary transfer if the mode is never set.
        // The harness answers `200` to any TYPE, so nothing else here can
        // notice its absence.
        let issued = log.lock().await.clone();
        assert_eq!(
            issued,
            vec![
                "USER tester".to_string(),
                "PASS pw".to_string(),
                "TYPE I".to_string(),
                "QUIT".to_string(),
            ],
            "connect should log in, set binary mode, then quit",
        );
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
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        transport
            .download("/big.bin", &local, Some(tx))
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

        // Issuing SIZE is only half of it — its answer has to reach the
        // progress channel, or the bar has a real number it never shows.
        // The sender was moved into `download` and dropped when it
        // returned, so nothing is still in flight and `try_recv` drains
        // the queue without an await that could hang.
        let mut updates = Vec::new();
        while let Ok(u) = rx.try_recv() {
            updates.push(u);
        }
        assert!(
            !updates.is_empty(),
            "a download given a progress sender must report at least once",
        );
        // `bytes_total` is fixed for the whole transfer, so every update
        // must carry SIZE's answer. Asserting on all of them rather than on
        // how many arrived: the chunking that decides the count is not what
        // is under test, and would make this brittle for no gain.
        assert!(
            updates
                .iter()
                .all(|u| u.bytes_total == payload.len() as u64),
            "every update must carry SIZE's answer as the total; got {:?}",
            updates.iter().map(|u| u.bytes_total).collect::<Vec<_>>(),
        );
        assert_eq!(
            updates.last().unwrap().bytes_done,
            payload.len() as u64,
            "the last update must account for every byte written",
        );
    }

    #[tokio::test]
    async fn uploading_preserves_every_byte() {
        let payload = pseudo_random(150_000);
        let dir = tempdir_for_test("upload");
        let local = dir.join("up.bin");
        std::fs::write(&local, &payload).unwrap();

        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport
            .upload(&local, "/up.bin", None)
            .await
            .expect("upload should succeed");

        let files = store.lock().await;
        assert_eq!(files.get("/up.bin").map(Vec::as_slice), Some(&payload[..]));
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

        let meta = transport
            .metadata("/a.txt")
            .await
            .unwrap()
            .expect("present");
        assert_eq!(meta.size, 12);
    }

    /// A `.part` file from an interrupted download must continue from its
    /// length via REST, not restart and not concatenate.
    #[tokio::test]
    async fn a_partial_download_resumes_from_its_offset() {
        let payload = pseudo_random(100_000);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/resume.bin".to_string(), payload.clone());

        let dir = tempdir_for_test("resume-download");
        let local = dir.join("resume.bin");

        // Seed the partial exactly as an interrupted download leaves it —
        // BOTH the bytes and the provenance sidecar. `decide_resume` refuses
        // to continue a partial it cannot identify, so without the sidecar
        // this test would silently exercise a fresh download instead, and
        // still pass: restarting from zero also lands the correct bytes.
        // That is why the REST assertion below is the real assertion.
        std::fs::write(crate::transport::part_path(&local), &payload[..30_000]).unwrap();
        crate::transport::write_part_meta(&local, "/resume.bin", Some(payload.len() as u64)).await;

        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport
            .download("/resume.bin", &local, None)
            .await
            .unwrap();

        let got = std::fs::read(&local).unwrap();
        assert_eq!(got.len(), payload.len(), "resumed file must be whole");
        assert_eq!(got, payload, "resumed bytes must match, not duplicate");

        let issued = log.lock().await.clone();
        assert!(
            issued.iter().any(|c| c == "REST 30000"),
            "the download must resume from the partial's length, not restart; \
             commands issued: {issued:?}",
        );
    }

    /// The four mutating commands, driven through the transport API. Each
    /// assertion reads the server's own store rather than the client's return
    /// value: a command that answered success without touching anything —
    /// an `MKD` arm that replies `257` and creates no key — must not pass.
    #[tokio::test]
    async fn rename_mkdir_rmdir_and_delete_reach_the_server() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/old.txt".to_string(), b"body".to_vec());
            files.insert("/doomed.txt".to_string(), b"x".to_vec());
            files.insert("/emptydir/".to_string(), Vec::new());
        }

        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport.rename("/old.txt", "/new.txt").await.unwrap();
        transport.mkdir("/fresh").await.unwrap();
        transport.delete_file("/doomed.txt").await.unwrap();
        transport.delete_dir("/emptydir", false).await.unwrap();

        let files = store.lock().await;
        assert!(files.contains_key("/new.txt"), "rename should move the key");
        assert!(!files.contains_key("/old.txt"), "old name should be gone");
        assert_eq!(files.get("/new.txt").unwrap().as_slice(), b"body");
        assert!(
            files.contains_key("/fresh/"),
            "mkdir should create a dir key"
        );
        assert!(!files.contains_key("/doomed.txt"), "delete should remove");
        assert!(!files.contains_key("/emptydir/"), "rmdir should remove");
        drop(files);

        // The store alone cannot distinguish `RMD /emptydir` from a `DELE`
        // that happened to remove the same key, so pin the verbs too.
        let issued = log.lock().await.clone();
        for expected in [
            "RNFR /old.txt",
            "RNTO /new.txt",
            "MKD /fresh",
            "DELE /doomed.txt",
            "RMD /emptydir",
        ] {
            assert!(
                issued.iter().any(|c| c == expected),
                "{expected} should have been issued; commands issued: {issued:?}",
            );
        }
    }

    /// The same verbs against paths that are not there. The harness answers
    /// `550` whenever the key is absent, and `map_ftp` reads 550 as
    /// [`crate::error::BlinkError::NotFound`] — so what is pinned here is the
    /// *variant*, not merely that something failed. A 550 misfiled as
    /// `Transport` or `Disconnected` would send the TUI down the "the link
    /// broke" path instead of telling the user the file isn't there.
    #[tokio::test]
    async fn renaming_removing_and_deleting_a_missing_path_each_report_not_found() {
        // Deliberately empty: every key the operations below name is absent.
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let renamed = transport.rename("/absent.txt", "/wherever.txt").await;
        assert!(
            matches!(renamed, Err(crate::error::BlinkError::NotFound(_))),
            "renaming a file that is not there should be NotFound; got {renamed:?}",
        );

        let removed = transport.delete_dir("/absentdir", false).await;
        assert!(
            matches!(removed, Err(crate::error::BlinkError::NotFound(_))),
            "removing a directory that is not there should be NotFound; got {removed:?}",
        );

        let deleted = transport.delete_file("/absent.txt").await;
        assert!(
            matches!(deleted, Err(crate::error::BlinkError::NotFound(_))),
            "deleting a file that is not there should be NotFound; got {deleted:?}",
        );

        // Each error has to be the server's `550` coming back, not a
        // client-side guard that refused before issuing anything — those
        // produce a different variant for a different reason.
        let issued = log.lock().await.clone();
        for expected in ["RNTO /wherever.txt", "RMD /absentdir", "DELE /absent.txt"] {
            assert!(
                issued.iter().any(|c| c == expected),
                "{expected} should have been issued; commands issued: {issued:?}",
            );
        }
    }

    /// Fixture for the two recursive-delete tests. `/sibling.txt` sits
    /// outside the tree on purpose: a walk that over-reaches takes it too.
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

    /// A store holding `n` sibling directories under `/wide`, and nothing
    /// else. Only directories accumulate on the delete walk's stack — files
    /// are unlinked inline — so width is what drives it towards the ceiling.
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

    /// A logged-in control connection, for the paths that take a raw stream
    /// rather than a `FtpTransport`.
    async fn raw_stream(port: u16) -> suppaftp::tokio::AsyncFtpStream {
        let mut stream = suppaftp::tokio::AsyncFtpStream::connect(&format!("127.0.0.1:{port}"))
            .await
            .expect("connect");
        stream.login("tester", "pw").await.expect("login");
        stream
    }

    /// The guard itself: a listing wider than the ceiling must stop the walk
    /// rather than let the stack grow with it. Run at a small limit so the
    /// behaviour is visible without a tree of `MAX_QUEUED_JOBS` entries;
    /// `a_recursive_delete_refuses_a_tree_wider_than_the_real_cap` pins that
    /// the production entry point supplies the real one.
    #[tokio::test]
    async fn a_recursive_delete_stops_once_pending_work_passes_the_cap() {
        let store = wide_store(4).await;
        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let mut stream = raw_stream(port).await;

        let err = super::ftp_delete_dir_capped(&mut stream, "/wide", true, 2)
            .await
            .expect_err("a listing wider than the ceiling must be refused");

        assert!(
            err.to_string().contains("exceeded") && err.to_string().contains('2'),
            "expected the delete budget error naming the limit, got: {err}",
        );

        // The guard has to fire before the walk acts, not after: a partially
        // deleted tree would be worse than a refused one.
        let issued = log.lock().await.clone();
        assert!(
            !issued.iter().any(|c| c.starts_with("RMD")),
            "nothing should have been removed; commands issued: {issued:?}",
        );
    }

    /// The production entry point must supply the *real* ceiling, not merely
    /// have one. Cheap despite its size: with the guard in place the walk
    /// stops on the iteration after the first listing, so this issues one
    /// LIST and removes nothing.
    #[tokio::test]
    async fn a_recursive_delete_refuses_a_tree_wider_than_the_real_cap() {
        let store = wide_store(crate::transfer::MAX_QUEUED_JOBS + 1).await;
        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let mut stream = raw_stream(port).await;

        let err = super::ftp_delete_dir(&mut stream, "/wide", true)
            .await
            .expect_err("a tree this wide must not be walked");

        assert!(
            err.to_string()
                .contains(&crate::transfer::MAX_QUEUED_JOBS.to_string()),
            "the error must name the real ceiling, got: {err}",
        );

        let issued = log.lock().await.clone();
        assert!(
            !issued.iter().any(|c| c.starts_with("RMD")),
            "nothing should have been removed; commands issued: {issued:?}",
        );
    }

    /// Index of `cmd` in the issued log, or a failure naming what was issued.
    fn issued_at(issued: &[String], cmd: &str) -> usize {
        issued
            .iter()
            .position(|c| c == cmd)
            .unwrap_or_else(|| panic!("{cmd} was never issued; commands issued: {issued:?}"))
    }

    /// `delete_dir(.., true)` — the tree walk Task 6 left uncovered, having
    /// exercised only `recursive: false`. The store shows *what* survived;
    /// only the log shows the order, and the order is the contract: `RMD` on
    /// a directory still holding entries draws a 550 from a real server.
    #[tokio::test]
    async fn a_recursive_delete_removes_a_nested_tree_bottom_up() {
        let store = tree_store().await;
        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

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

        let issued = log.lock().await.clone();
        assert!(
            issued_at(&issued, "DELE /tree/sub/b.txt") < issued_at(&issued, "RMD /tree/sub"),
            "a directory must be emptied before it is removed; commands issued: {issued:?}",
        );
        assert!(
            issued_at(&issued, "RMD /tree/sub") < issued_at(&issued, "RMD /tree"),
            "the child directory must go before its parent; commands issued: {issued:?}",
        );
    }

    /// The walk's two skip-and-warn guards, whose comments claim one hostile
    /// entry should not strand the rest. The server appends names the store
    /// cannot hold: `.` and `..`, a `/` that `join_remote` refuses to join,
    /// and a NUL-bearing name that `check_ftp_path` refuses to send. Acting
    /// on any of them is a command the log will show; failing on any of them
    /// leaves the tree half-deleted.
    #[tokio::test]
    async fn a_recursive_delete_skips_hostile_entries_and_finishes() {
        let store = tree_store().await;
        let faults = Faults {
            hostile_listing_names: true,
            ..Faults::default()
        };
        let (port, _c, log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport
            .delete_dir("/tree", true)
            .await
            .expect("a hostile entry should be skipped, not fail the whole delete");

        let mut remaining: Vec<String> = store.lock().await.keys().cloned().collect();
        remaining.sort();
        assert_eq!(
            remaining,
            vec!["/sibling.txt".to_string()],
            "the hostile entries must not stop the tree from being removed",
        );

        let issued = log.lock().await.clone();
        assert!(
            !issued.iter().any(|c| c.contains('\0')),
            "a NUL must never reach the control channel; commands issued: {issued:?}",
        );
        // Bare `/tree` is in the list because it is what the historical bug
        // produced: an unjoinable name folded onto the directory being
        // walked, so the walk acted on its own parent instead of skipping.
        for forbidden in [
            "DELE /tree/..",
            "RMD /tree/..",
            "DELE /tree/.",
            "RMD /tree/.",
            "DELE /",
            "RMD /",
            "DELE /tree",
        ] {
            assert!(
                !issued.iter().any(|c| c == forbidden),
                "{forbidden} should never be issued; commands issued: {issued:?}",
            );
        }
    }

    /// Bounds a raw-socket protocol exchange so a test that drives `handle_control`
    /// by hand fails loudly instead of hanging CI forever when the expected reply
    /// never comes (e.g. because the command under test isn't implemented yet).
    async fn with_timeout<F: std::future::Future>(fut: F, what: &str) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(5), fut)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// APPE must extend the remote file, not truncate it.
    #[tokio::test]
    async fn appending_extends_rather_than_truncating() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/app.bin".to_string(), b"first-".to_vec());

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        // Drive APPE directly: the client-side resume policy is not what is
        // under test here, the server contract is.
        let mut sock = with_timeout(TcpStream::connect(("127.0.0.1", port)), "control connect")
            .await
            .unwrap();
        let (r, mut w) = sock.split();
        let mut lines = BufReader::new(r).lines();
        with_timeout(lines.next_line(), "220 banner").await.unwrap(); // 220

        w.write_all(b"PASV\r\n").await.unwrap();
        let pasv_line = with_timeout(lines.next_line(), "227 PASV reply")
            .await
            .unwrap()
            .unwrap();
        let data_port = parse_pasv_port(&pasv_line);

        w.write_all(b"APPE /app.bin\r\n").await.unwrap();
        with_timeout(lines.next_line(), "150 for APPE")
            .await
            .unwrap(); // 150
        let mut data = with_timeout(TcpStream::connect(("127.0.0.1", data_port)), "data connect")
            .await
            .unwrap();
        data.write_all(b"second").await.unwrap();
        data.shutdown().await.unwrap();
        drop(data);
        with_timeout(lines.next_line(), "226 after APPE")
            .await
            .unwrap(); // 226

        let files = store.lock().await;
        assert_eq!(files.get("/app.bin").unwrap().as_slice(), b"first-second");
    }

    /// A failed `RETR` (no such file) must still consume the restart marker,
    /// the same as a successful one — otherwise a `REST` left over from an
    /// aborted transfer silently offsets the next, unrelated `RETR`. Task 3
    /// originally consumed `rest` only on RETR's success path, after the
    /// early `550`/`425` returns; this drives the raw protocol to prove the
    /// marker does not survive a failure.
    #[tokio::test]
    async fn a_failed_retr_still_consumes_the_restart_marker() {
        let payload = pseudo_random(500);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/whole.bin".to_string(), payload.clone());

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        let mut sock = with_timeout(TcpStream::connect(("127.0.0.1", port)), "control connect")
            .await
            .unwrap();
        let (r, mut w) = sock.split();
        let mut lines = BufReader::new(r).lines();
        with_timeout(lines.next_line(), "220 banner").await.unwrap(); // 220

        // Set a restart marker, then fail the transfer it would have applied to.
        w.write_all(b"REST 100\r\n").await.unwrap();
        with_timeout(lines.next_line(), "350 after REST")
            .await
            .unwrap(); // 350

        w.write_all(b"PASV\r\n").await.unwrap();
        let pasv_line = with_timeout(lines.next_line(), "227 PASV reply (first)")
            .await
            .unwrap()
            .unwrap();
        let _unused_data_port = parse_pasv_port(&pasv_line);

        w.write_all(b"RETR /does-not-exist.bin\r\n").await.unwrap();
        let reply = with_timeout(lines.next_line(), "550 for missing file")
            .await
            .unwrap()
            .unwrap();
        assert!(reply.starts_with("550"), "expected 550, got {reply:?}");

        // A fresh, unrelated RETR must not be offset by the stale marker.
        w.write_all(b"PASV\r\n").await.unwrap();
        let pasv_line = with_timeout(lines.next_line(), "227 PASV reply (second)")
            .await
            .unwrap()
            .unwrap();
        let data_port = parse_pasv_port(&pasv_line);

        w.write_all(b"RETR /whole.bin\r\n").await.unwrap();
        with_timeout(lines.next_line(), "150 for the good RETR")
            .await
            .unwrap(); // 150

        let mut data = with_timeout(TcpStream::connect(("127.0.0.1", data_port)), "data connect")
            .await
            .unwrap();
        let mut got = Vec::new();
        with_timeout(data.read_to_end(&mut got), "RETR body")
            .await
            .unwrap();
        drop(data);
        with_timeout(lines.next_line(), "226 after the good RETR")
            .await
            .unwrap(); // 226

        assert_eq!(
            got, payload,
            "a failed RETR must not leave a stale REST offsetting the next RETR"
        );
    }

    /// An out-of-range PASV octet must surface as an error, not a panic and
    /// not a hang. suppaftp 10.0 changed this from a panic; these tests pin
    /// the behaviour on both sides of that bump.
    #[tokio::test]
    async fn a_malformed_pasv_reply_is_an_error_not_a_panic() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/a.txt".to_string(), b"x".to_vec());

        let faults = Faults {
            bad_pasv_octet: true,
            ..Faults::default()
        };
        let (port, _c, log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let result = transport.list("/").await;
        assert!(result.is_err(), "a 999 octet must not be accepted");

        // Without the fault the store's one entry would come back and the
        // assertion above would fail, so this cannot pass with the injection
        // switched off; the log pins that it got as far as PASV.
        let issued = log.lock().await.clone();
        assert!(
            issued.iter().any(|c| c == "PASV"),
            "PASV should have been issued; commands issued: {issued:?}",
        );
    }

    /// A garbage body must not become an entry. `File::from_str` would let it:
    /// it falls through to the MLSX parsers, which split on `;` and name the
    /// file the last token, so any non-empty line parses. `ftp_list` calls the
    /// LIST parsers directly instead, and an unparsable line is skipped.
    #[tokio::test]
    async fn an_unparsable_listing_line_becomes_no_entry_at_all() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/a.txt".to_string(), b"x".to_vec());

        let faults = Faults {
            unparsable_list_line: true,
            ..Faults::default()
        };
        let (port, _c, log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        // Either an error or an empty listing is acceptable — a panic is not,
        // and neither is a garbage entry addressed at a nonexistent path.
        match transport.list("/").await {
            Err(_) => {}
            Ok(entries) => assert!(
                entries.is_empty(),
                "an unparsable line must not become an entry: {entries:?}"
            ),
        }

        // An empty listing only means anything if the garbage body is what
        // emptied it: the store holds `/a.txt`, which an unfaulted server
        // would list, so this pins that LIST was reached and asked for the
        // directory holding it rather than the exchange failing earlier.
        let issued = log.lock().await.clone();
        assert!(
            issued.iter().any(|c| c == "LIST /"),
            "LIST / should have been issued; commands issued: {issued:?}",
        );
    }

    #[tokio::test]
    async fn a_connection_dropped_mid_transfer_is_reported_not_hung() {
        let payload = pseudo_random(50_000);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store.lock().await.insert("/gone.bin".to_string(), payload);

        let faults = Faults {
            abrupt_close: true,
            ..Faults::default()
        };
        let (port, _c, log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let dir = tempdir_for_test("abrupt-close");
        let local = dir.join("gone.bin");

        // Must not hang: FTP_OP_TIMEOUT is 60s, so a 10s bound proves the
        // failure comes from the closed socket rather than the deadline.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            transport.download("/gone.bin", &local, None),
        )
        .await;

        let inner = result.expect("must fail fast, not wait out the timeout");
        assert!(inner.is_err(), "a dropped connection must be an error");

        // The store holds the file, so without the fault this download would
        // succeed and the assertion above would fail; the log pins that the
        // server dropped the socket at RETR rather than never being asked.
        let issued = log.lock().await.clone();
        assert!(
            issued.iter().any(|c| c == "RETR /gone.bin"),
            "RETR should have been issued; commands issued: {issued:?}",
        );
        assert!(
            !local.exists(),
            "a failed download must not land under the final name",
        );
    }

    /// Hitting the preview cap is not a dropped connection. It used to be
    /// reported as one — the cap was raised as `FtpError::ConnectionError`,
    /// which `map_ftp` classifies as `Disconnected` — and raising it from the
    /// `retr` callback also skipped finalisation, so on suppaftp 10 the
    /// connection refused every later data command. The preview runs on the
    /// browsing connection, so the next listing has to work.
    #[tokio::test]
    async fn an_oversized_preview_is_a_transport_error_and_the_connection_survives() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert(
                "/big.bin".to_string(),
                vec![7u8; super::MAX_PREVIEW_BYTES as usize + 1024],
            );
            files.insert("/small.txt".to_string(), b"hello".to_vec());
        }
        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let err = with_timeout(transport.read_to_bytes("/big.bin"), "oversized preview")
            .await
            .expect_err("a file over the cap must not preview");
        assert!(
            matches!(err, crate::error::BlinkError::Transport(_)),
            "the cap is not a disconnect: {err:?}",
        );
        assert!(err.to_string().contains("preview size limit"), "{err}");

        let entries = with_timeout(transport.list("/"), "list after the preview")
            .await
            .expect("the connection must still serve a listing");
        assert_eq!(entries.len(), 2);
        let small = with_timeout(transport.read_to_bytes("/small.txt"), "second preview")
            .await
            .expect("and a preview under the cap");
        assert_eq!(&small[..], b"hello");
    }

    /// `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)` -> port.
    fn parse_pasv_port(line: &str) -> u16 {
        let inner = line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inner, _)| inner)
            .expect("PASV reply should carry a tuple");
        let parts: Vec<u16> = inner
            .split(',')
            .map(|p| p.trim().parse().unwrap())
            .collect();
        parts[4] * 256 + parts[5]
    }
}
