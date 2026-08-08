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

/// The on-disk path a download writes to while it's in flight.
///
/// We always stream into `<final>.part` and rename onto the final name only
/// once the transfer has completed and been fsynced. That way:
///
/// - An interrupted download leaves the partial bytes under a distinguishable
///   suffix instead of next to the user's pre-existing real file.
/// - Resume code can identify the partial unambiguously (the bare final
///   filename never holds half a download).
/// - On power loss after rename, the parent-directory fsync in
///   [`crate::paths::sync_parent_dir`] guarantees the rename is durable.
pub(crate) fn part_path(local: &Path) -> PathBuf {
    let mut s = local.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Sidecar recording which remote file a `.part` holds bytes of.
///
/// See [`decide_resume`] for why bytes alone are not enough to resume.
pub(crate) fn part_meta_path(local: &Path) -> PathBuf {
    let mut s = part_path(local).into_os_string();
    s.push(".meta");
    PathBuf::from(s)
}

/// Provenance of a partial download, stored next to the `.part` file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PartMeta {
    /// The remote path these bytes came from.
    pub remote_path: String,
    /// The size the server reported when the partial was started, if it
    /// reported one. A change means the file was replaced between attempts.
    pub size: Option<u64>,
}

/// What to do with an existing `.part` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeDecision {
    /// Discard any partial and download from byte zero.
    Fresh,
    /// Continue from this offset.
    Resume(u64),
}

/// Decide whether an existing partial download can be continued.
///
/// A `.part` file records bytes and nothing else, so length alone cannot
/// answer "are these the right bytes?". Two downloads that land on the same
/// local name — `/a/report.pdf` interrupted, then `/b/report.pdf` started —
/// produce one partial and one resume that appends the second file's tail to
/// the first file's head, fsyncs it, and renames it into place looking like
/// a completed download. The corruption is silent and survives.
///
/// So resume requires positive identification: a sidecar naming the same
/// remote path, and a server-reported size that hasn't moved since. Anything
/// unproven restarts. Restarting costs bandwidth; resuming the wrong bytes
/// costs the user a corrupt file they have no reason to re-check.
///
/// Pure so the policy can be tested without touching a filesystem or a
/// server; [`resume_offset`] does the I/O around it.
pub(crate) fn decide_resume(
    part_len: Option<u64>,
    meta: Option<&PartMeta>,
    remote_path: &str,
    reported_size: Option<u64>,
) -> ResumeDecision {
    // No partial, or an empty one — nothing to continue.
    let Some(part_len) = part_len.filter(|n| *n > 0) else {
        return ResumeDecision::Fresh;
    };

    // Unidentified bytes: a partial from an older blink, or one whose
    // sidecar was lost. We cannot tell what it is, so we don't trust it.
    let Some(meta) = meta else {
        return ResumeDecision::Fresh;
    };

    if meta.remote_path != remote_path {
        return ResumeDecision::Fresh;
    }

    // The file was replaced between attempts: same path, different content.
    if let (Some(then), Some(now)) = (meta.size, reported_size)
        && then != now
    {
        return ResumeDecision::Fresh;
    }

    // More bytes than the file has: the remote shrank, or the partial is
    // not what it claims.
    if let Some(now) = reported_size
        && part_len > now
    {
        return ResumeDecision::Fresh;
    }

    ResumeDecision::Resume(part_len)
}

/// Resolve the resume offset for a download, cleaning up a stale partial.
///
/// Returns the byte offset to start from. On [`ResumeDecision::Fresh`] the
/// existing `.part` and its sidecar are removed, so the caller can create
/// the file from scratch.
pub(crate) async fn resume_offset(
    local_path: &Path,
    remote_path: &str,
    reported_size: Option<u64>,
) -> u64 {
    let part = part_path(local_path);
    let meta_path = part_meta_path(local_path);

    let part_len = tokio::fs::metadata(&part).await.ok().map(|m| m.len());
    let meta: Option<PartMeta> = match tokio::fs::read(&meta_path).await {
        Ok(raw) => serde_json::from_slice(&raw).ok(),
        Err(_) => None,
    };

    match decide_resume(part_len, meta.as_ref(), remote_path, reported_size) {
        ResumeDecision::Resume(offset) => offset,
        ResumeDecision::Fresh => {
            if part_len.is_some() {
                tracing::debug!(
                    part = %part.display(),
                    remote = %remote_path,
                    "discarding a partial that cannot be identified as this file",
                );
            }
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_file(&meta_path).await;
            0
        }
    }
}

/// Record which remote file the in-flight `.part` belongs to.
///
/// Best-effort: a failure here costs a restart on the next attempt, never
/// correctness, because a missing sidecar reads as "unidentified" and forces
/// a fresh download.
pub(crate) async fn write_part_meta(
    local_path: &Path,
    remote_path: &str,
    reported_size: Option<u64>,
) {
    let meta = PartMeta {
        remote_path: remote_path.to_string(),
        size: reported_size,
    };
    if let Ok(raw) = serde_json::to_vec(&meta) {
        let _ = tokio::fs::write(part_meta_path(local_path), raw).await;
    }
}

/// Drop the sidecar once the download has been renamed into place.
pub(crate) async fn clear_part_meta(local_path: &Path) {
    let _ = tokio::fs::remove_file(part_meta_path(local_path)).await;
}

pub(crate) mod error_map;
pub mod ftp;
pub(crate) mod ftp_impl;
pub mod ftps;
pub mod scp;
pub mod sftp;

/// One entry from a remote directory listing.
///
/// The name is deliberately split in two, because the string that is safe to
/// *render* is not the string that is safe to *address*.
///
/// [`crate::error::sanitize`] replaces control and bidi-format characters
/// with a space and truncates past a length cap — necessary before a
/// server-controlled name reaches the terminal, and lossy by construction.
/// A sanitized name therefore identifies a different file than the one the
/// server listed, or no file at all; worse, two distinct names can sanitize
/// to the same string, so an operation aimed at one can land on the other.
///
/// Keeping both under distinct names means the compiler asks the question at
/// every use site: rendering takes [`Self::display_name`], and anything that
/// builds a path — `join_remote`, download, delete, rename — takes
/// [`Self::raw_name`].
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    /// The name exactly as the server sent it. Use for every path.
    pub raw_name: String,
    /// Sanitized for terminal rendering. Never use to address anything.
    pub display_name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Populated by SFTP/SCP; `None` for FTP (protocol doesn't report it in LIST).
    /// Not yet rendered in the file pane — reserved for a future column.
    #[allow(dead_code)]
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
    /// POSIX mode bits; `None` for FTP. Reserved for a future permissions column.
    #[allow(dead_code)]
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
    /// Build an entry from the name the server reported, deriving the
    /// rendered form from it.
    ///
    /// This is the only constructor transports should use: it makes the
    /// sanitized name impossible to forget and impossible to drift from the
    /// raw one.
    pub fn new(
        raw_name: String,
        kind: EntryKind,
        size: u64,
        modified: Option<chrono::DateTime<chrono::Utc>>,
        mode: Option<u32>,
    ) -> Self {
        let display_name = crate::error::sanitize(raw_name.clone());
        Self {
            raw_name,
            display_name,
            kind,
            size,
            modified,
            mode,
        }
    }

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

/// Maximum time allowed for `transport::open` (TCP connect + SSH handshake +
/// auth). Shared between the TUI initial-connect path and the dispatcher's
/// per-job connect path so both enforce the same deadline.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What every protocol implementation must provide.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Human-readable label, e.g. `Protocol::Sftp`.
    #[allow(dead_code)]
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

/// Result of [`open`]: the live transport plus any side-channel info the
/// caller may want to persist back onto the session.
pub struct Connected {
    pub transport: Box<dyn Transport>,
    /// Hex SHA-256 of the FTPS server's leaf certificate, set only when an
    /// FTPS connect with `accept_invalid_certs=true` captured a new pin
    /// (TOFU). The caller should write this into `session.cert_sha256` and
    /// save the session.
    pub new_cert_pin: Option<String>,
}

/// Build the right transport for `session`. The password (if any) must be
/// resolved by the caller before this is invoked — we never store it on disk.
///
/// `app_event_tx` is forwarded to the SFTP/SCP handler for the host-key
/// confirmation flow. FTP/FTPS do not use host-key verification.
///
/// `trust` carries the keys the user accepted for this session without
/// saving them. It must be the *same* store for every connection a connected
/// session opens — the interactive one and each transfer worker's — or an
/// "accept once" is re-asked per connection. See
/// [`crate::known_hosts::SessionTrust`].
pub async fn open(
    session: &Session,
    password: Option<&str>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    trust: crate::known_hosts::SessionTrust,
) -> Result<Connected> {
    let (transport, new_cert_pin): (Box<dyn Transport>, Option<String>) = match session.protocol {
        Protocol::Sftp => (
            Box::new(sftp::SftpTransport::connect(session, password, app_event_tx, trust).await?),
            None,
        ),
        Protocol::Scp => (
            Box::new(scp::ScpTransport::connect(session, password, app_event_tx, trust).await?),
            None,
        ),
        Protocol::Ftp => (
            Box::new(ftp::FtpTransport::connect(session, password).await?),
            None,
        ),
        Protocol::Ftps => {
            let (t, pin) = ftps::FtpsTransport::connect(session, password).await?;
            (Box::new(t), pin)
        }
    };
    Ok(Connected {
        transport,
        new_cert_pin,
    })
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
    // Reject `.` and `..` components: `..` traverses upward; `.` is a no-op
    // but would produce paths like `/foo/./bar` that some servers don't
    // normalise, and a server-controlled `.` in a name is almost always
    // malicious.
    if name.split('/').any(|c| c == ".." || c == ".") {
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

/// In-memory mock transport for testing transfer logic without a real server.
///
/// Stores files in a `HashMap<String, Vec<u8>>` keyed by remote path.
/// Directory structure is implicit — any path can be listed if it was created
/// via `mkdir`, and any path can hold a file via `upload`.
#[cfg(test)]
pub(crate) mod mock {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use tokio::sync::mpsc;

    use crate::error::Result;
    use crate::session::Protocol;
    use crate::transport::{EntryKind, ProgressUpdate, RemoteEntry, Transport};

    #[derive(Debug, Clone)]
    pub struct MockTransport {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        dirs: Arc<Mutex<Vec<String>>>,
    }

    impl MockTransport {
        #[allow(dead_code)]
        pub fn new() -> Self {
            Self {
                files: Arc::new(Mutex::new(HashMap::new())),
                dirs: Arc::new(Mutex::new(vec!["/".to_string()])),
            }
        }

        #[allow(dead_code)]
        pub fn with_file(self, path: &str, contents: &[u8]) -> Self {
            let mut parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("/");
            if parent.is_empty() {
                parent = "/";
            }
            self.dirs.lock().unwrap().push(parent.to_string());
            self.files.lock().unwrap().insert(path.to_string(), contents.to_vec());
            self
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        fn protocol(&self) -> Protocol {
            Protocol::Sftp
        }

        async fn list(&mut self, remote_path: &str) -> Result<Vec<RemoteEntry>> {
            let p = if remote_path.ends_with('/') {
                remote_path.to_string()
            } else {
                format!("{remote_path}/")
            };
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();

            if !dirs.contains(&remote_path.to_string()) && remote_path != "/" {
                return Err(crate::error::BlinkError::transport(format!(
                    "no such directory: {remote_path}"
                )));
            }

            let mut entries: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for path in files.keys() {
                if let Some(rest) = path.strip_prefix(&p)
                    && let Some(name) = rest.split('/').next()
                    && !name.is_empty() {
                        entries.insert(name.to_string());
                    }
            }
            for dir in dirs.iter() {
                if let Some(rest) = dir.strip_prefix(&p)
                    && let Some(name) = rest.split('/').next()
                    && !name.is_empty() {
                        entries.insert(name.to_string());
                    }
            }

            let mut out = Vec::new();
            for name in entries {
                let is_dir = {
                    let full = format!("{}{}", p, name);
                    dirs.contains(&full)
                };
                let size = if is_dir {
                    0
                } else {
                    let full = format!("{}{}", p, name);
                    files.get(&full).map(|b| b.len() as u64).unwrap_or(0)
                };
                out.push(RemoteEntry::new(
                    name,
                    if is_dir {
                        EntryKind::Directory
                    } else {
                        EntryKind::File
                    },
                    size,
                    None,
                    None,
                ));
            }
            Ok(out)
        }

        async fn download(
            &mut self,
            remote_path: &str,
            local_path: &Path,
            progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> Result<()> {
            let data = {
                let files = self.files.lock().unwrap();
                files
                    .get(remote_path)
                    .cloned()
                    .ok_or_else(|| {
                        crate::error::BlinkError::transport(format!(
                            "file not found: {remote_path}"
                        ))
                    })?
            };
            if let Some(parent) = local_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(local_path, &data).await?;
            if let Some(tx) = &progress {
                let _ = tx.send(ProgressUpdate {
                    bytes_done: data.len() as u64,
                    bytes_total: data.len() as u64,
                });
            }
            Ok(())
        }

        async fn upload(
            &mut self,
            local_path: &Path,
            remote_path: &str,
            _progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> Result<()> {
            let data = tokio::fs::read(local_path).await?;
            self.files
                .lock()
                .unwrap()
                .insert(remote_path.to_string(), data);
            Ok(())
        }

        async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
            let mut files = self.files.lock().unwrap();
            if let Some(data) = files.remove(from) {
                files.insert(to.to_string(), data);
                Ok(())
            } else {
                Err(crate::error::BlinkError::transport(format!(
                    "file not found: {from}"
                )))
            }
        }

        async fn delete_file(&mut self, remote_path: &str) -> Result<()> {
            self.files.lock().unwrap().remove(remote_path);
            Ok(())
        }

        async fn delete_dir(&mut self, remote_path: &str, recursive: bool) -> Result<()> {
            let mut dirs = self.dirs.lock().unwrap();
            if recursive {
                dirs.retain(|d| !d.starts_with(remote_path));
                self.files.lock().unwrap().retain(|k, _| {
                    !k.starts_with(remote_path)
                });
            } else {
                dirs.retain(|d| d != remote_path);
            }
            Ok(())
        }

        async fn mkdir(&mut self, remote_path: &str) -> Result<()> {
            self.dirs
                .lock()
                .unwrap()
                .push(remote_path.to_string());
            Ok(())
        }

        async fn metadata(&mut self, remote_path: &str) -> Result<Option<RemoteEntry>> {
            let files = self.files.lock().unwrap();
            let dirs = self.dirs.lock().unwrap();
            if let Some(data) = files.get(remote_path) {
                let name = remote_path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(remote_path)
                    .to_string();
                return Ok(Some(RemoteEntry::new(
                    name,
                    EntryKind::File,
                    data.len() as u64,
                    None,
                    None,
                )));
            }
            if dirs.contains(&remote_path.to_string()) {
                let name = remote_path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(remote_path)
                    .to_string();
                return Ok(Some(RemoteEntry::new(
                    name,
                    EntryKind::Directory,
                    0,
                    None,
                    None,
                )));
            }
            Ok(None)
        }

        async fn read_to_bytes(&mut self, remote_path: &str) -> Result<Bytes> {
            let files = self.files.lock().unwrap();
            files
                .get(remote_path)
                .cloned()
                .map(Bytes::from)
                .ok_or_else(|| {
                    crate::error::BlinkError::transport(format!(
                        "file not found: {remote_path}"
                    ))
                })
        }

        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- resume provenance -------------------------------------------------
    //
    // A `.part` file records only bytes, not which remote file they came
    // from. Resuming on length alone means an interrupted download of one
    // file can be "completed" with the tail of a different file that happens
    // to share a local name — silently, and with a successful-looking rename
    // at the end. These pin the identity check that makes resume safe.

    fn meta(remote: &str, size: Option<u64>) -> PartMeta {
        PartMeta {
            remote_path: remote.to_string(),
            size,
        }
    }

    #[test]
    fn resumes_a_partial_of_the_same_remote_file() {
        let d = decide_resume(Some(4_000), Some(&meta("/a/report.pdf", Some(9_000))), "/a/report.pdf", Some(9_000));
        assert_eq!(d, ResumeDecision::Resume(4_000));
    }

    #[test]
    fn restarts_when_the_partial_belongs_to_a_different_remote_file() {
        // The bug this exists for: same local name, different source. The
        // old code appended file B onto file A's bytes and renamed the
        // result into place as a completed download.
        let d = decide_resume(Some(4_000), Some(&meta("/a/report.pdf", Some(9_000))), "/b/report.pdf", Some(9_000));
        assert_eq!(d, ResumeDecision::Fresh, "a partial of another file must not be resumed");
    }

    #[test]
    fn restarts_when_the_partial_has_no_provenance() {
        // A `.part` left by an older blink, or one whose sidecar was lost.
        // Nothing identifies it, so it cannot be trusted.
        let d = decide_resume(Some(4_000), None, "/a/report.pdf", Some(9_000));
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn restarts_when_the_remote_file_changed_size_since_the_partial() {
        // Same path, but the file was replaced between attempts.
        let d = decide_resume(Some(4_000), Some(&meta("/a/report.pdf", Some(9_000))), "/a/report.pdf", Some(12_000));
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn restarts_when_the_partial_is_longer_than_the_remote_file() {
        let d = decide_resume(Some(20_000), Some(&meta("/a/report.pdf", Some(9_000))), "/a/report.pdf", Some(9_000));
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn starts_fresh_when_there_is_no_partial() {
        assert_eq!(
            decide_resume(None, None, "/a/report.pdf", Some(9_000)),
            ResumeDecision::Fresh
        );
    }

    #[test]
    fn resumes_with_an_unknown_remote_size_when_provenance_matches() {
        // FTP servers may not answer SIZE. The old guard was written
        // `total > 0 && offset > total`, so an unknown size skipped the
        // staleness check entirely and resumed unconditionally. Identity is
        // checked independently of size, so this is now safe — and a
        // mismatched path is still refused (next test).
        let d = decide_resume(Some(4_000), Some(&meta("/a/report.pdf", None)), "/a/report.pdf", None);
        assert_eq!(d, ResumeDecision::Resume(4_000));
    }

    #[test]
    fn restarts_with_an_unknown_remote_size_when_provenance_differs() {
        let d = decide_resume(Some(4_000), Some(&meta("/a/report.pdf", None)), "/b/report.pdf", None);
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn empty_partial_starts_fresh() {
        let d = decide_resume(Some(0), Some(&meta("/a/report.pdf", Some(9_000))), "/a/report.pdf", Some(9_000));
        assert_eq!(d, ResumeDecision::Fresh, "nothing to resume from");
    }

    #[test]
    fn part_meta_path_sits_beside_the_partial() {
        assert_eq!(
            part_meta_path(Path::new("/tmp/file.iso")),
            PathBuf::from("/tmp/file.iso.part.meta")
        );
    }

    // part_path
    #[test]
    fn part_path_appends_suffix() {
        assert_eq!(
            part_path(Path::new("/tmp/file.iso")),
            PathBuf::from("/tmp/file.iso.part")
        );
    }

    #[test]
    fn part_path_preserves_compound_extensions() {
        // foo.tar.gz -> foo.tar.gz.part (not foo.tar.part)
        assert_eq!(
            part_path(Path::new("/tmp/archive.tar.gz")),
            PathBuf::from("/tmp/archive.tar.gz.part")
        );
    }

    #[test]
    fn part_path_with_no_extension() {
        assert_eq!(
            part_path(Path::new("/tmp/README")),
            PathBuf::from("/tmp/README.part")
        );
    }

    // join_remote
    #[test]
    fn join_appends_name() {
        assert_eq!(join_remote("/home/user", "file.txt"), "/home/user/file.txt");
    }

    #[test]
    fn join_trailing_slash_base() {
        assert_eq!(join_remote("/home/user/", "file.txt"), "/home/user/file.txt");
    }

    #[test]
    fn join_strips_leading_slash_from_name() {
        assert_eq!(join_remote("/srv", "/etc/shadow"), "/srv/etc/shadow");
    }

    #[test]
    fn join_rejects_dotdot_traversal() {
        assert_eq!(join_remote("/srv/data", "../secret"), "/srv/data");
    }

    #[test]
    fn join_rejects_embedded_dotdot() {
        assert_eq!(join_remote("/srv/data", "a/../b"), "/srv/data");
    }

    #[test]
    fn join_rejects_single_dot() {
        assert_eq!(join_remote("/srv/data", "."), "/srv/data");
    }

    #[test]
    fn join_rejects_embedded_single_dot() {
        assert_eq!(join_remote("/srv/data", "a/./b"), "/srv/data");
    }

    #[test]
    fn join_root_base() {
        assert_eq!(join_remote("/", "etc"), "/etc");
    }

    // parent_remote
    #[test]
    fn parent_of_root_is_root() {
        assert_eq!(parent_remote("/"), "/");
    }

    #[test]
    fn parent_of_file_in_root() {
        assert_eq!(parent_remote("/file.txt"), "/");
    }

    #[test]
    fn parent_of_nested_path() {
        assert_eq!(parent_remote("/home/user/docs"), "/home/user");
    }

    #[test]
    fn parent_strips_trailing_slash() {
        assert_eq!(parent_remote("/home/user/docs/"), "/home/user");
    }

    #[test]
    fn parent_of_empty_is_root() {
        assert_eq!(parent_remote(""), "/");
    }

    // -----------------------------------------------------------------------
    // MockTransport tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mock_list_empty() {
        let mut m = mock::MockTransport::new();
        let entries = m.list("/").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn mock_list_with_file() {
        let mut m = mock::MockTransport::new().with_file("/hello.txt", b"world");
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].raw_name, "hello.txt");
        assert!(!entries[0].is_dir());
        assert_eq!(entries[0].size, 5);
    }

    #[tokio::test]
    async fn mock_list_with_dir() {
        let mut m = mock::MockTransport::new();
        m.mkdir("/subdir").await.unwrap();
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].raw_name, "subdir");
    }

    #[tokio::test]
    async fn mock_upload_and_download() {
        let dir = std::env::temp_dir().join("blink-mock-test");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let local = dir.join(format!("upload-{}", std::process::id()));

        let mut m = mock::MockTransport::new();
        tokio::fs::write(&local, b"hello from mock").await.unwrap();
        m.upload(&local, "/remote.txt", None).await.unwrap();

        let dest = dir.join("downloaded.txt");
        m.download("/remote.txt", &dest, None).await.unwrap();
        let data = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(data, b"hello from mock");

        let _ = tokio::fs::remove_file(&local).await;
        let _ = tokio::fs::remove_file(&dest).await;
    }

    #[tokio::test]
    async fn mock_rename() {
        let mut m = mock::MockTransport::new().with_file("/old.txt", b"data");
        m.rename("/old.txt", "/new.txt").await.unwrap();
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].raw_name, "new.txt");
        assert!(m.metadata("/old.txt").await.unwrap().is_none());
        assert!(m.metadata("/new.txt").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn mock_delete() {
        let mut m = mock::MockTransport::new()
            .with_file("/a.txt", b"aaa")
            .with_file("/b.txt", b"bbb");
        m.delete_file("/a.txt").await.unwrap();
        let entries = m.list("/").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].raw_name, "b.txt");
    }

    #[tokio::test]
    async fn mock_delete_dir_recursive() {
        let mut m = mock::MockTransport::new();
        m.mkdir("/dir").await.unwrap();
        let mut inner = mock::MockTransport::new();
        inner.mkdir("/dir/sub").await.unwrap();
        // Add a file inside the subdirectory via the shared transport
        m = inner;
        m.delete_dir("/dir", true).await.unwrap();
        assert!(m.metadata("/dir").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mock_read_to_bytes() {
        let mut m = mock::MockTransport::new().with_file("/data.bin", b"\x00\x01\x02");
        let bytes = m.read_to_bytes("/data.bin").await.unwrap();
        assert_eq!(&bytes[..], &[0, 1, 2]);
    }

    #[tokio::test]
    async fn mock_metadata_file() {
        let mut m = mock::MockTransport::new().with_file("/f", b"12345");
        let meta = m.metadata("/f").await.unwrap().unwrap();
        assert_eq!(meta.raw_name, "f");
        assert!(!meta.is_dir());
        assert_eq!(meta.size, 5);
    }

    #[tokio::test]
    async fn mock_metadata_not_found() {
        let mut m = mock::MockTransport::new();
        assert!(m.metadata("/nope").await.unwrap().is_none());
    }
}

