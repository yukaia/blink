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

/// Suffix of the files a transfer writes while it is in flight: a local
/// partial for a download, a remote one for an upload. See [`part_name`].
///
/// Specific to blink on purpose. It used to be `.part`, which browsers and
/// other tools use too, and blink deleted or truncated a file of that name
/// that happened to sit beside a download — or, for an upload, on the
/// server. No one else writes `.blink-part`.
const PART_SUFFIX: &str = ".blink-part";

/// Suffix of a partial's provenance sidecar. See [`part_meta_path`].
const META_SUFFIX: &str = ".meta";

/// Longest file name, in bytes, that most filesystems accept: ext4, XFS,
/// btrfs, APFS and ZFS all stop at 255. NTFS counts 255 UTF-16 units, and
/// no character takes more of those than it takes UTF-8 bytes, so a name
/// within this many bytes fits there too.
const MAX_NAME_BYTES: usize = 255;

/// The name of the partial for a file named `name`:
/// `<name>.<hash>.blink-part`, where `<hash>` is the first 8 hex digits of
/// the SHA-256 of `name_bytes`, the name's exact bytes.
///
/// The hash keeps the partial from being any file's own name. It used to be
/// plain `<name>.blink-part`, which is also the name of a file called that:
/// downloading `foo` beside a downloaded `foo.blink-part` deleted it as a
/// stale partial, and parallel jobs for the two wrote one file. A file now
/// shares its name with a partial only if it carries that partial's hash.
///
/// `name` is shortened, on a character boundary, as far as the partial's
/// sidecar needs to stay within [`MAX_NAME_BYTES`]; the hash covers the
/// whole name, so two names alike up to the cut still differ. `name` is
/// display only and may be lossy; only `name_bytes` decides the hash.
fn part_name(name_bytes: &[u8], name: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let hash = Sha256::digest(name_bytes);
    let mut tag = String::with_capacity(8);
    for b in &hash[..4] {
        let _ = write!(&mut tag, "{b:02x}");
    }
    let budget = MAX_NAME_BYTES - META_SUFFIX.len() - PART_SUFFIX.len() - tag.len() - 1;
    let mut cut = name.len().min(budget);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}.{tag}{PART_SUFFIX}", &name[..cut])
}

/// The on-disk path a download writes to while it's in flight, beside
/// `local` and named by [`part_name`].
///
/// We always stream into the partial and rename onto the final name only
/// once the transfer has completed and been fsynced. That way:
///
/// - An interrupted download leaves the partial bytes under a distinguishable
///   name instead of next to the user's pre-existing real file.
/// - Resume code can identify the partial unambiguously (the bare final
///   filename never holds half a download).
/// - On power loss after rename, the parent-directory fsync in
///   [`crate::paths::sync_parent_dir`] guarantees the rename is durable.
pub(crate) fn part_path(local: &Path) -> PathBuf {
    let name = local.file_name().unwrap_or_default();
    local.with_file_name(part_name(name.as_encoded_bytes(), &name.to_string_lossy()))
}

/// The remote path an upload writes to while it's in flight; renamed onto
/// `remote` once the whole file is stored. Named as [`part_path`] names a
/// local partial.
pub(crate) fn remote_part_path(remote: &str) -> String {
    match remote.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/{}", part_name(name.as_bytes(), name)),
        None => part_name(remote.as_bytes(), remote),
    }
}

/// Sidecar recording which remote file a `.blink-part` holds bytes of.
///
/// See [`decide_resume`] for why bytes alone are not enough to resume.
pub(crate) fn part_meta_path(local: &Path) -> PathBuf {
    let mut s = part_path(local).into_os_string();
    s.push(META_SUFFIX);
    PathBuf::from(s)
}

/// Provenance of a partial download, stored next to the `.blink-part` file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PartMeta {
    /// The remote path these bytes came from.
    pub remote_path: String,
    /// The size the server reported when the partial was started, if it
    /// reported one. A change means the file was replaced between attempts.
    pub size: Option<u64>,
    /// The server and account the bytes came from, as [`origin_of`] names
    /// it. `None` in a sidecar written before origins were recorded, which
    /// is treated as unidentified.
    #[serde(default)]
    pub origin: Option<String>,
}

/// Where a download's bytes come from: `user@host:port`, host folded to
/// lower case. Two servers, or two accounts on one, can each hold a file at
/// the same path with the same size; without this, a download from one
/// could resume into a partial of the other's. The protocol is left out:
/// SFTP and SCP read the same files, and FTP and SFTP normally differ by
/// port anyway.
pub(crate) fn origin_of(session: &Session) -> String {
    format!(
        "{}@{}:{}",
        session.username,
        session.host.to_ascii_lowercase(),
        session.port
    )
}

/// What to do with an existing `.blink-part` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeDecision {
    /// Discard any partial and download from byte zero.
    Fresh,
    /// Continue from this offset.
    Resume(u64),
}

/// Decide whether an existing partial download can be continued.
///
/// A `.blink-part` file records bytes and nothing else, so length alone cannot
/// answer "are these the right bytes?". Two downloads that land on the same
/// local name — `/a/report.pdf` interrupted, then `/b/report.pdf` started —
/// produce one partial and one resume that appends the second file's tail to
/// the first file's head, fsyncs it, and renames it into place looking like
/// a completed download. The corruption is silent and survives.
///
/// So resume requires positive identification: a sidecar naming the same
/// origin and remote path, and a server-reported size that hasn't moved
/// since. Anything unproven restarts. Restarting costs bandwidth; resuming the wrong bytes
/// costs the user a corrupt file they have no reason to re-check.
///
/// Pure so the policy can be tested without touching a filesystem or a
/// server; [`resume_offset`] does the I/O around it.
pub(crate) fn decide_resume(
    part_len: Option<u64>,
    meta: Option<&PartMeta>,
    remote_path: &str,
    reported_size: Option<u64>,
    origin: &str,
) -> ResumeDecision {
    // No partial, or an empty one — nothing to continue.
    let Some(part_len) = part_len.filter(|n| *n > 0) else {
        return ResumeDecision::Fresh;
    };

    // Unidentified bytes: a partial whose sidecar was lost. We cannot tell
    // what it is, so we don't trust it.
    let Some(meta) = meta else {
        return ResumeDecision::Fresh;
    };

    // Bytes from another server or account, or a sidecar too old to say.
    if meta.origin.as_deref() != Some(origin) {
        return ResumeDecision::Fresh;
    }

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
/// existing `.blink-part` and its sidecar are removed, so the caller can create
/// the file from scratch.
pub(crate) async fn resume_offset(
    local_path: &Path,
    remote_path: &str,
    reported_size: Option<u64>,
    origin: &str,
) -> u64 {
    let part = part_path(local_path);
    let meta_path = part_meta_path(local_path);

    let part_len = tokio::fs::metadata(&part).await.ok().map(|m| m.len());
    let meta: Option<PartMeta> = match tokio::fs::read(&meta_path).await {
        Ok(raw) => serde_json::from_slice(&raw).ok(),
        Err(_) => None,
    };

    match decide_resume(part_len, meta.as_ref(), remote_path, reported_size, origin) {
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

/// Record which remote file the in-flight `.blink-part` belongs to.
///
/// Best-effort: a failure here costs a restart on the next attempt, never
/// correctness, because a missing sidecar reads as "unidentified" and forces
/// a fresh download.
pub(crate) async fn write_part_meta(
    local_path: &Path,
    remote_path: &str,
    reported_size: Option<u64>,
    origin: &str,
) {
    let meta = PartMeta {
        remote_path: remote_path.to_string(),
        size: reported_size,
        origin: Some(origin.to_string()),
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
/// Throwaway SSH keys used only by `sftp`'s integration tests.
#[cfg(test)]
mod sftp_test_keys;

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

/// A directory listing, and how many of its entries could not be read.
///
/// An FTP server answers `LIST` with text lines in a format blink has to
/// parse, and a line matching no format it knows is skipped. That used to
/// go only to the debug log, so the pane came up short, or empty, with no
/// reason given — and a recursive download silently missed those entries.
/// Carrying the count with the listing lets each caller say so. SFTP
/// listings are structured, so theirs is always zero.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub entries: Vec<RemoteEntry>,
    /// Entries the server listed that could not be read.
    pub unreadable: usize,
}

impl Listing {
    /// A listing every entry of which was read.
    pub fn complete(entries: Vec<RemoteEntry>) -> Self {
        Self {
            entries,
            unreadable: 0,
        }
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
///
/// It bounds the network, not the user: time spent with a host-key prompt
/// open does not count — see [`within_deadline_excluding_waits`].
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Tells the connect deadline when a connection is waiting on the user.
///
/// The SSH host-key handler holds one and marks the time it spends waiting
/// for an answer to its prompt; the caller that set the deadline holds the
/// receiver and stops the clock meanwhile. Without it, a user who took more
/// than the deadline to check a fingerprint — which the README tells them to
/// do out of band — got "connection timed out" under the open prompt.
#[derive(Clone)]
pub struct UserWait(tokio::sync::watch::Sender<bool>);

impl UserWait {
    /// A signal and the receiver for [`within_deadline_excluding_waits`].
    pub fn new() -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (Self(tx), rx)
    }

    /// Mark the connection as waiting on the user until the guard drops,
    /// on every path out: an answer, a timeout, a cancelled connect.
    pub(crate) fn waiting(&self) -> UserWaitGuard<'_> {
        self.0.send_replace(true);
        UserWaitGuard(&self.0)
    }
}

/// Ends a [`UserWait::waiting`] period on drop.
pub(crate) struct UserWaitGuard<'a>(&'a tokio::sync::watch::Sender<bool>);

impl Drop for UserWaitGuard<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

/// Run `fut` under a deadline of `limit` that counts only the time no one is
/// waiting on the user, as signalled through `waiting`. Returns `None` if
/// the deadline passes first.
///
/// The pause suspends the deadline rather than resetting it: time before
/// and after a prompt adds up. It ends when the signal is cleared, or when
/// every [`UserWait`] is gone — a pause nothing can end any more must not
/// last forever, so the remaining time applies from then.
pub(crate) async fn within_deadline_excluding_waits<F: std::future::Future>(
    limit: std::time::Duration,
    mut waiting: tokio::sync::watch::Receiver<bool>,
    fut: F,
) -> Option<F::Output> {
    tokio::pin!(fut);
    let mut remaining = limit;
    loop {
        let paused = *waiting.borrow_and_update();
        let started = tokio::time::Instant::now();
        let signal_lost = if paused {
            tokio::select! {
                out = &mut fut => return Some(out),
                changed = waiting.changed() => changed.is_err(),
            }
        } else {
            tokio::select! {
                out = &mut fut => return Some(out),
                () = tokio::time::sleep(remaining) => return None,
                changed = waiting.changed() => {
                    remaining = remaining.saturating_sub(started.elapsed());
                    changed.is_err()
                }
            }
        };
        if signal_lost {
            return tokio::time::timeout(remaining, fut).await.ok();
        }
    }
}

/// What every protocol implementation must provide.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Human-readable label, e.g. `Protocol::Sftp`.
    #[allow(dead_code)]
    fn protocol(&self) -> Protocol;

    /// List entries in `remote_path`. Implementations must NOT include `.` or `..`.
    async fn list(&mut self, remote_path: &str) -> Result<Listing>;

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
/// `user_wait` is how the SSH handler marks time spent on its host-key
/// prompt, so the caller's connect deadline can exclude it; see
/// [`within_deadline_excluding_waits`]. FTP/FTPS never prompt.
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
    user_wait: UserWait,
) -> Result<Connected> {
    let (transport, new_cert_pin): (Box<dyn Transport>, Option<String>) = match session.protocol {
        Protocol::Sftp => (
            Box::new(
                sftp::SftpTransport::connect(session, password, app_event_tx, trust, user_wait)
                    .await?,
            ),
            None,
        ),
        Protocol::Scp => (
            Box::new(
                scp::ScpTransport::connect(session, password, app_event_tx, trust, user_wait)
                    .await?,
            ),
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
/// resolution most servers apply.
///
/// Returns `None` when `name` cannot be joined safely, and the caller must
/// skip the entry. It used to return `base` unchanged in that case, which
/// callers could not tell apart from a successful join — so they acted on it.
/// The recursive delete was the sharp edge: for an entry named `..` it called
/// `remove_file` on the directory being walked rather than skipping the
/// entry.
pub(crate) fn join_remote(base: &str, name: &str) -> Option<String> {
    let name = name.trim_start_matches('/');
    if name.is_empty() {
        return None;
    }
    // Reject `.` and `..` components: `..` traverses upward; `.` is a no-op
    // but would produce paths like `/foo/./bar` that some servers don't
    // normalise, and a server-controlled `.` in a name is almost always
    // malicious.
    if name.split('/').any(|c| c == ".." || c == ".") {
        return None;
    }
    Some(if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    })
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
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), contents.to_vec());
            self
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        fn protocol(&self) -> Protocol {
            Protocol::Sftp
        }

        async fn list(&mut self, remote_path: &str) -> Result<crate::transport::Listing> {
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
                    && !name.is_empty()
                {
                    entries.insert(name.to_string());
                }
            }
            for dir in dirs.iter() {
                if let Some(rest) = dir.strip_prefix(&p)
                    && let Some(name) = rest.split('/').next()
                    && !name.is_empty()
                {
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
            Ok(crate::transport::Listing::complete(out))
        }

        async fn download(
            &mut self,
            remote_path: &str,
            local_path: &Path,
            progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> Result<()> {
            let data = {
                let files = self.files.lock().unwrap();
                files.get(remote_path).cloned().ok_or_else(|| {
                    crate::error::BlinkError::transport(format!("file not found: {remote_path}"))
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
                self.files
                    .lock()
                    .unwrap()
                    .retain(|k, _| !k.starts_with(remote_path));
            } else {
                dirs.retain(|d| d != remote_path);
            }
            Ok(())
        }

        async fn mkdir(&mut self, remote_path: &str) -> Result<()> {
            self.dirs.lock().unwrap().push(remote_path.to_string());
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
                    crate::error::BlinkError::transport(format!("file not found: {remote_path}"))
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

    // -- the connect deadline and the host-key prompt ----------------------
    //
    // The connect deadline bounds the network: a server that accepts the
    // socket and then stalls. It must not bound the user, who may be checking
    // a host-key fingerprint through another channel before answering the
    // prompt — the README tells them to. Time with the prompt open does not
    // count. The paused clock is safe here: nothing below does real I/O.

    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn a_connect_that_finishes_in_time_returns_its_result() {
        let (_wait, waiting) = UserWait::new();
        let out = within_deadline_excluding_waits(Duration::from_secs(30), waiting, async {
            tokio::time::sleep(Duration::from_secs(29)).await;
            7
        })
        .await;
        assert_eq!(out, Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn time_outside_a_prompt_counts_toward_the_deadline() {
        let (_wait, waiting) = UserWait::new();
        let out = within_deadline_excluding_waits(Duration::from_secs(30), waiting, async {
            tokio::time::sleep(Duration::from_secs(31)).await;
        })
        .await;
        assert_eq!(out, None, "a stalled server must still time out");
    }

    #[tokio::test(start_paused = true)]
    async fn time_with_a_prompt_open_does_not_count() {
        let (wait, waiting) = UserWait::new();
        // 5 s of network, a 30 s prompt, 5 s more: 40 s in all, 10 counted.
        let out = within_deadline_excluding_waits(Duration::from_secs(30), waiting, async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            {
                let _open = wait.waiting();
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            "connected"
        })
        .await;
        assert_eq!(out, Some("connected"));
    }

    #[tokio::test(start_paused = true)]
    async fn time_before_and_after_a_prompt_adds_up() {
        let (wait, waiting) = UserWait::new();
        // 20 s, a prompt, then 11 s: 31 s of network time.
        let out = within_deadline_excluding_waits(Duration::from_secs(30), waiting, async {
            tokio::time::sleep(Duration::from_secs(20)).await;
            {
                let _open = wait.waiting();
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            tokio::time::sleep(Duration::from_secs(11)).await;
        })
        .await;
        assert_eq!(out, None, "the prompt pauses the deadline, not resets it");
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_signal_mid_prompt_restores_the_deadline() {
        let (wait, waiting) = UserWait::new();
        let out = within_deadline_excluding_waits(Duration::from_secs(30), waiting, async move {
            let open = wait.waiting();
            // Leak the guard so the flag is never cleared, then drop every
            // sender: nothing can end the pause any more.
            std::mem::forget(open);
            drop(wait);
            tokio::time::sleep(Duration::from_secs(3600)).await;
        })
        .await;
        assert_eq!(out, None, "a pause nothing can end must not last forever");
    }

    // -- resume provenance -------------------------------------------------
    //
    // A `.blink-part` file records only bytes, not which remote file they came
    // from. Resuming on length alone means an interrupted download of one
    // file can be "completed" with the tail of a different file that happens
    // to share a local name — silently, and with a successful-looking rename
    // at the end. These pin the identity check that makes resume safe.

    /// The server the tests below download from.
    const ORIGIN: &str = "me@files.example:22";

    fn meta(remote: &str, size: Option<u64>) -> PartMeta {
        PartMeta {
            remote_path: remote.to_string(),
            size,
            origin: Some(ORIGIN.to_string()),
        }
    }

    /// `decide_resume` for a download from [`ORIGIN`].
    fn decide(
        part_len: Option<u64>,
        meta: Option<&PartMeta>,
        remote_path: &str,
        reported_size: Option<u64>,
    ) -> ResumeDecision {
        decide_resume(part_len, meta, remote_path, reported_size, ORIGIN)
    }

    // Two servers — or two accounts on one — can each hold a file at the
    // same path with the same size. Path and size alone let a download from
    // one resume into a partial of the other's; the sidecar also records
    // where the bytes came from.

    #[test]
    fn restarts_when_the_partial_came_from_another_server() {
        let m = PartMeta {
            origin: Some("me@other.example:22".into()),
            ..meta("/a/report.pdf", Some(9_000))
        };
        assert_eq!(
            decide(Some(4_000), Some(&m), "/a/report.pdf", Some(9_000)),
            ResumeDecision::Fresh,
        );
    }

    #[test]
    fn restarts_when_the_partial_came_from_another_account_or_port() {
        for origin in ["you@files.example:22", "me@files.example:2222"] {
            let m = PartMeta {
                origin: Some(origin.into()),
                ..meta("/a/report.pdf", Some(9_000))
            };
            assert_eq!(
                decide(Some(4_000), Some(&m), "/a/report.pdf", Some(9_000)),
                ResumeDecision::Fresh,
                "{origin}",
            );
        }
    }

    /// A sidecar written before origins were recorded cannot say where its
    /// bytes came from, so it is as unidentified as no sidecar at all.
    #[test]
    fn restarts_when_the_sidecar_predates_origins() {
        let m = PartMeta {
            origin: None,
            ..meta("/a/report.pdf", Some(9_000))
        };
        assert_eq!(
            decide(Some(4_000), Some(&m), "/a/report.pdf", Some(9_000)),
            ResumeDecision::Fresh,
        );
    }

    #[test]
    fn a_sidecar_without_an_origin_still_parses() {
        let old: PartMeta =
            serde_json::from_str(r#"{"remote_path":"/a/report.pdf","size":9000}"#).unwrap();
        assert_eq!(old.origin, None);
    }

    #[test]
    fn the_origin_names_user_host_and_port_with_the_host_folded() {
        let mut s = crate::session::Session::from_url("sftp://me@Files.Example:2222/").unwrap();
        assert_eq!(origin_of(&s), "me@files.example:2222");
        s.protocol = crate::session::Protocol::Scp;
        assert_eq!(
            origin_of(&s),
            "me@files.example:2222",
            "SFTP and SCP read the same files",
        );
    }

    #[test]
    fn resumes_a_partial_of_the_same_remote_file() {
        let d = decide(
            Some(4_000),
            Some(&meta("/a/report.pdf", Some(9_000))),
            "/a/report.pdf",
            Some(9_000),
        );
        assert_eq!(d, ResumeDecision::Resume(4_000));
    }

    #[test]
    fn restarts_when_the_partial_belongs_to_a_different_remote_file() {
        // The bug this exists for: same local name, different source. The
        // old code appended file B onto file A's bytes and renamed the
        // result into place as a completed download.
        let d = decide(
            Some(4_000),
            Some(&meta("/a/report.pdf", Some(9_000))),
            "/b/report.pdf",
            Some(9_000),
        );
        assert_eq!(
            d,
            ResumeDecision::Fresh,
            "a partial of another file must not be resumed"
        );
    }

    #[test]
    fn restarts_when_the_partial_has_no_provenance() {
        // A `.blink-part` whose sidecar was lost. Nothing identifies it, so it
        // cannot be trusted.
        let d = decide(Some(4_000), None, "/a/report.pdf", Some(9_000));
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn restarts_when_the_remote_file_changed_size_since_the_partial() {
        // Same path, but the file was replaced between attempts.
        let d = decide(
            Some(4_000),
            Some(&meta("/a/report.pdf", Some(9_000))),
            "/a/report.pdf",
            Some(12_000),
        );
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn restarts_when_the_partial_is_longer_than_the_remote_file() {
        let d = decide(
            Some(20_000),
            Some(&meta("/a/report.pdf", Some(9_000))),
            "/a/report.pdf",
            Some(9_000),
        );
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn starts_fresh_when_there_is_no_partial() {
        assert_eq!(
            decide(None, None, "/a/report.pdf", Some(9_000)),
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
        let d = decide(
            Some(4_000),
            Some(&meta("/a/report.pdf", None)),
            "/a/report.pdf",
            None,
        );
        assert_eq!(d, ResumeDecision::Resume(4_000));
    }

    #[test]
    fn restarts_with_an_unknown_remote_size_when_provenance_differs() {
        let d = decide(
            Some(4_000),
            Some(&meta("/a/report.pdf", None)),
            "/b/report.pdf",
            None,
        );
        assert_eq!(d, ResumeDecision::Fresh);
    }

    #[test]
    fn empty_partial_starts_fresh() {
        let d = decide(
            Some(0),
            Some(&meta("/a/report.pdf", Some(9_000))),
            "/a/report.pdf",
            Some(9_000),
        );
        assert_eq!(d, ResumeDecision::Fresh, "nothing to resume from");
    }

    /// The final component of `path`, as a `&str`.
    fn file_name(path: &Path) -> &str {
        path.file_name().unwrap().to_str().unwrap()
    }

    #[test]
    fn part_meta_path_sits_beside_the_partial() {
        let local = Path::new("/tmp/file.iso");
        let mut expected = part_path(local).into_os_string();
        expected.push(".meta");
        assert_eq!(part_meta_path(local), PathBuf::from(expected));
    }

    // part_path
    #[test]
    fn a_partial_keeps_its_files_name_and_directory() {
        let part = part_path(Path::new("/tmp/archive.tar.gz"));
        assert_eq!(part.parent(), Some(Path::new("/tmp")));
        let name = file_name(&part);
        let hash = name
            .strip_prefix("archive.tar.gz.")
            .and_then(|rest| rest.strip_suffix(".blink-part"))
            .unwrap_or_else(|| panic!("unexpected partial name {name}"));
        assert_eq!(hash.len(), 8, "{name}");
        assert!(
            hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{name}"
        );
    }

    /// Resume finds the partial by computing its name again.
    #[test]
    fn a_partials_name_is_the_same_every_time() {
        let local = Path::new("/tmp/README");
        assert_eq!(part_path(local), part_path(local));
    }

    /// A file whose own name ends in the partial suffix used to share its
    /// name with the partial of the file it was named after: downloading
    /// `foo` deleted a downloaded `foo.blink-part`, and two parallel jobs
    /// for the pair wrote one file.
    #[test]
    fn a_file_named_like_a_partial_is_not_that_partial() {
        let foo = Path::new("/d/foo");
        let lookalike = Path::new("/d/foo.blink-part");
        assert_ne!(part_path(foo), lookalike);
        assert_ne!(part_path(foo), part_path(lookalike));
        assert_ne!(remote_part_path("/d/foo"), "/d/foo.blink-part");
    }

    #[tokio::test]
    async fn resuming_a_download_leaves_a_file_named_like_its_partial_alone() {
        let home = crate::paths::test_home();
        let local = home.path().join("foo");
        let lookalike = home.path().join("foo.blink-part");
        std::fs::write(&lookalike, b"a file of its own").unwrap();

        let offset = resume_offset(&local, "/r/foo", Some(10), "origin").await;

        assert_eq!(offset, 0);
        assert_eq!(std::fs::read(&lookalike).unwrap(), b"a file of its own");
    }

    #[test]
    fn a_remote_partial_is_named_as_a_local_one_is() {
        let remote = remote_part_path("/srv/data.csv");
        let local = part_path(Path::new("/srv/data.csv"));
        assert_eq!(remote, local.to_str().unwrap());
        assert_eq!(
            remote_part_path("data.csv"),
            file_name(&part_path(Path::new("data.csv")))
        );
    }

    /// Most filesystems cap a name at 255 bytes. A name near the cap used to
    /// leave no room for the suffix, so its partial (or sidecar) could not
    /// be created.
    #[test]
    fn a_partial_of_a_name_at_the_length_cap_fits_under_it() {
        let long = "a".repeat(255);
        let part = part_path(&Path::new("/d").join(&long));
        assert!(file_name(&part).len() <= 255, "{}", file_name(&part).len());
        let meta = part_meta_path(&Path::new("/d").join(&long));
        assert!(file_name(&meta).len() <= 255, "{}", file_name(&meta).len());
        let remote = remote_part_path(&format!("/d/{long}"));
        assert!(remote.len() - "/d/".len() <= 255, "{}", remote.len());
    }

    /// Shortening must not split a character: the name stays valid UTF-8,
    /// so it can be created on any filesystem and sent to any server.
    #[test]
    fn a_shortened_partial_name_keeps_whole_characters() {
        let long = "é".repeat(127); // 254 bytes, two per character
        let remote = remote_part_path(&format!("/d/{long}"));
        let name = remote.strip_prefix("/d/").unwrap();
        assert!(name.len() <= 255 - ".meta".len(), "{}", name.len());
        assert!(name.starts_with("éé"), "{name}");
    }

    /// Two long names that differ only past the point where they are
    /// shortened still get partials of their own.
    #[test]
    fn long_names_alike_until_their_ends_get_distinct_partials() {
        let a = format!("{}-a", "x".repeat(250));
        let b = format!("{}-b", "x".repeat(250));
        assert_ne!(
            part_path(&Path::new("/d").join(a)),
            part_path(&Path::new("/d").join(b))
        );
    }

    /// blink's partials are named so they cannot be mistaken for anyone
    /// else's. `.part` is what browsers and other tools use, and blink used
    /// to delete or truncate a file of that name next to a download.
    #[test]
    fn a_partial_is_not_named_like_other_tools_partials() {
        let local = Path::new("/tmp/report.pdf");
        assert_ne!(part_path(local), PathBuf::from("/tmp/report.pdf.part"));
        assert_ne!(remote_part_path("/r/report.pdf"), "/r/report.pdf.part");
    }

    // join_remote
    #[test]
    fn join_appends_name() {
        assert_eq!(
            join_remote("/home/user", "file.txt").as_deref(),
            Some("/home/user/file.txt")
        );
    }

    #[test]
    fn join_trailing_slash_base() {
        assert_eq!(
            join_remote("/home/user/", "file.txt").as_deref(),
            Some("/home/user/file.txt")
        );
    }

    #[test]
    fn join_strips_leading_slash_from_name() {
        assert_eq!(
            join_remote("/srv", "/etc/shadow").as_deref(),
            Some("/srv/etc/shadow")
        );
    }

    // Returning the base unchanged made "rejected" indistinguishable from a
    // real join, so callers acted on it: the recursive delete walked its
    // entries and called `remove_file(parent_dir)` for a hostile name
    // instead of skipping it. `None` forces every caller to decide.

    #[test]
    fn join_rejects_dotdot_traversal() {
        assert_eq!(join_remote("/srv/data", "../secret"), None);
    }

    #[test]
    fn join_rejects_embedded_dotdot() {
        assert_eq!(join_remote("/srv/data", "a/../b"), None);
    }

    #[test]
    fn join_rejects_single_dot() {
        assert_eq!(join_remote("/srv/data", "."), None);
    }

    #[test]
    fn join_rejects_embedded_single_dot() {
        assert_eq!(join_remote("/srv/data", "a/./b"), None);
    }

    #[test]
    fn join_root_base() {
        assert_eq!(join_remote("/", "etc").as_deref(), Some("/etc"));
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
        let entries = m.list("/").await.unwrap().entries;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn mock_list_with_file() {
        let mut m = mock::MockTransport::new().with_file("/hello.txt", b"world");
        let entries = m.list("/").await.unwrap().entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].raw_name, "hello.txt");
        assert!(!entries[0].is_dir());
        assert_eq!(entries[0].size, 5);
    }

    #[tokio::test]
    async fn mock_list_with_dir() {
        let mut m = mock::MockTransport::new();
        m.mkdir("/subdir").await.unwrap();
        let entries = m.list("/").await.unwrap().entries;
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
        let entries = m.list("/").await.unwrap().entries;
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
        let entries = m.list("/").await.unwrap().entries;
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
