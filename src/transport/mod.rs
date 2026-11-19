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
    hashed_name(name_bytes, name, PART_SUFFIX)
}

/// Suffix of the name a remote file is moved to while an upload replaces
/// it. See [`remote_aside_path`].
const ASIDE_SUFFIX: &str = ".blink-old";

/// `<name>.<hash><suffix>`, as [`part_name`] describes. `suffix` is no
/// longer than [`PART_SUFFIX`], which the length budget allows for.
fn hashed_name(name_bytes: &[u8], name: &str, suffix: &str) -> String {
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
    format!("{}.{tag}{suffix}", &name[..cut])
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
///   [`finish_download`] guarantees the rename is durable.
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

/// Where the file at `remote` waits while an upload is renamed onto its
/// name, on a server whose rename won't replace it: beside it, named as
/// [`remote_part_path`] names a partial but ending `.blink-old`.
///
/// The file used to be deleted to make room. When the rename then failed
/// again, for whatever made it fail the first time, the old file was gone
/// and the new one sat under its partial's name. Moved aside, it can be
/// put back.
pub(crate) fn remote_aside_path(remote: &str) -> String {
    match remote.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/{}", hashed_name(name.as_bytes(), name, ASIDE_SUFFIX)),
        None => hashed_name(remote.as_bytes(), remote, ASIDE_SUFFIX),
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
    /// The modification time the server reported when the partial was
    /// started, in seconds since the Unix epoch. Catches a replacement of
    /// the same size, which `size` cannot. `None` in a sidecar written
    /// before mtimes were recorded, which then restarts once.
    #[serde(default)]
    pub mtime: Option<u64>,
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

/// What the server said about a remote file as a download opened it: the
/// two facts that change when the file is replaced. Either can be unknown —
/// an FTP server may not answer `SIZE` or `MDTM` — and an unknown one
/// cannot vouch for a partial.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RemoteStamp {
    pub size: Option<u64>,
    /// Seconds since the Unix epoch.
    pub mtime: Option<u64>,
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
/// origin and remote path, and a server-reported size and modification time
/// that both match what the sidecar recorded. Anything unproven restarts,
/// including a size or mtime unknown on either side: a server that can't
/// say what the file is now can't show it wasn't replaced. Restarting costs
/// bandwidth; resuming the wrong bytes costs the user a corrupt file they
/// have no reason to re-check.
///
/// Pure so the policy can be tested without touching a filesystem or a
/// server; [`resume_offset`] does the I/O around it.
pub(crate) fn decide_resume(
    part_len: Option<u64>,
    meta: Option<&PartMeta>,
    remote_path: &str,
    now: RemoteStamp,
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

    // The file may have been replaced between attempts: same path,
    // different content. Unknown on either side proves nothing either way.
    let (Some(size), Some(mtime)) = (now.size, now.mtime) else {
        return ResumeDecision::Fresh;
    };
    if meta.size != Some(size) || meta.mtime != Some(mtime) {
        return ResumeDecision::Fresh;
    }

    // More bytes than the file has: the partial is not what it claims.
    if part_len > size {
        return ResumeDecision::Fresh;
    }

    ResumeDecision::Resume(part_len)
}

/// Resolve the resume offset for a download, cleaning up a stale partial.
///
/// Returns the byte offset to start from. On [`ResumeDecision::Fresh`] the
/// existing `.blink-part` and its sidecar are removed, so the caller can create
/// the file from scratch. A partial that is there and can't be removed fails
/// the download, naming it: in a sticky folder such as `/tmp`, one another
/// user left there can't be, and the exclusive create would otherwise fail
/// with a bare "File exists".
///
/// Only a partial and sidecar that are [`ours`] count: in a folder other
/// users can write to, one of them could plant both, the sidecar naming a
/// public file's origin, path, size and mtime. Resumed, the download would
/// append the real file's tail to their bytes and rename the result into
/// place, still theirs to rewrite.
pub(crate) async fn resume_offset(
    local_path: &Path,
    remote_path: &str,
    now: RemoteStamp,
    origin: &str,
) -> crate::error::Result<u64> {
    let part = part_path(local_path);
    let meta_path = part_meta_path(local_path);

    // `symlink_metadata`, not `metadata`: a symlink planted under the
    // partial's name is not a partial, whatever it points at. It reads as
    // "no partial" and is removed below like any other stale one.
    let part_len = tokio::fs::symlink_metadata(&part)
        .await
        .ok()
        .filter(ours)
        .map(|m| m.len());
    let meta: Option<PartMeta> = match read_sidecar(&meta_path).await {
        Ok(raw) => serde_json::from_slice(&raw).ok(),
        Err(_) => None,
    };

    match decide_resume(part_len, meta.as_ref(), remote_path, now, origin) {
        ResumeDecision::Resume(offset) => Ok(offset),
        ResumeDecision::Fresh => {
            if part_len.is_some() {
                tracing::debug!(
                    part = %part.display(),
                    remote = %remote_path,
                    "discarding a partial that cannot be identified as this file",
                );
            }
            if let Err(e) = remove_partial(local_path).await {
                return Err(crate::error::BlinkError::transport(format!(
                    "{}: a stale partial download is in the way and can't be removed ({e}); \
                     delete it, or download somewhere else",
                    part.display()
                )));
            }
            Ok(0)
        }
    }
}

/// Whether `m` describes a file blink can take as its own partial or
/// sidecar: a regular file, and on Unix one the user running blink owns,
/// with no other name. See [`resume_offset`]. A second name would be a hard
/// link to some other file, which appending would change too.
///
/// Windows has no single owner id to compare, and needs privilege to make
/// the links, so there only the file type is checked.
fn ours(m: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        owned_single_file(
            m.is_file(),
            m.uid(),
            m.nlink(),
            rustix::process::geteuid().as_raw(),
        )
    }
    #[cfg(not(unix))]
    {
        m.is_file()
    }
}

/// The rule [`ours`] applies on Unix, apart from the lookups.
#[cfg_attr(not(unix), allow(dead_code))]
fn owned_single_file(is_file: bool, owner: u32, links: u64, me: u32) -> bool {
    is_file && owner == me && links == 1
}

/// Record which remote file the in-flight `.blink-part` belongs to.
///
/// Best-effort: a failure here costs a restart on the next attempt, never
/// correctness, because a missing sidecar reads as "unidentified" and forces
/// a fresh download.
pub(crate) async fn write_part_meta(
    local_path: &Path,
    remote_path: &str,
    stamp: RemoteStamp,
    origin: &str,
) {
    let meta = PartMeta {
        remote_path: remote_path.to_string(),
        size: stamp.size,
        mtime: stamp.mtime,
        origin: Some(origin.to_string()),
    };
    let Ok(raw) = serde_json::to_vec(&meta) else {
        return;
    };
    // Remove, then create exclusively: `create_new` refuses an existing
    // path, symlink included, so a sidecar planted between the two makes
    // this fail (and the next attempt restart) instead of writing through
    // the link. Removing a symlink removes the link, not its target.
    let path = part_meta_path(local_path);
    let _ = tokio::fs::remove_file(&path).await;
    let written = async {
        let mut f = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut f, &raw).await?;
        // A tokio `File` hands each write to a blocking thread and returns;
        // without this the write could still be in flight when the file is
        // dropped, its error lost, and the sidecar read back empty.
        tokio::io::AsyncWriteExt::flush(&mut f).await
    }
    .await;
    if let Err(e) = written {
        tracing::debug!(path = %path.display(), error = %e, "could not write the partial's sidecar");
    }
}

/// Open the `.blink-part` file a download writes into.
///
/// The partial's name is predictable, so in a directory other users can
/// write to (`/tmp`, a shared drop folder) someone could plant a symlink
/// there and have the download truncate or append to whatever it points
/// at. So neither open follows one:
///
/// - `offset == 0`: [`resume_offset`] has just removed any old partial, so
///   create exclusively. `create_new` fails on any existing path, a symlink
///   included.
/// - `offset > 0`: append, with `O_NOFOLLOW` on Unix so a symlink swapped
///   in after `resume_offset` looked fails the open. Windows needs
///   privilege to create symlinks, so it gets no equivalent.
///
/// Takes the partial's [`PartLease`], which every write through the
/// returned [`PartFile`] holds until it lands.
pub(crate) async fn open_part(
    lease: &PartLease,
    part: &Path,
    offset: u64,
) -> std::io::Result<PartFile> {
    let mut opts = tokio::fs::OpenOptions::new();
    if offset > 0 {
        opts.append(true);
        #[cfg(unix)]
        opts.custom_flags(libc::O_NOFOLLOW);
    } else {
        opts.write(true).create_new(true);
    }
    let file = opts.open(part).await?.into_std().await;
    Ok(PartFile {
        file: std::sync::Arc::new(file),
        lease: lease.clone(),
        pending: None,
    })
}

/// Exclusive use of one `.blink-part` path, from before a download measures
/// it until after its rename. See [`lease_part`].
///
/// Clones share the hold: it ends when the last one drops, and each write a
/// [`PartFile`] has in flight owns one.
#[derive(Clone)]
pub(crate) struct PartLease(#[allow(dead_code)] std::sync::Arc<tokio::sync::OwnedMutexGuard<()>>);

/// One lock per partial path with a lease held or waited for. Entries are
/// weak, so a path no one is using drops out on the next lease.
static PART_LOCKS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(Default::default);

/// Wait for exclusive use of the partial at `part`, then take it.
///
/// A file write runs on a blocking thread, and ending the task that asked
/// for it does not end the write: a cancelled job's abort, or an attempt
/// that failed on the network, drops the file with its last write still
/// going. The next attempt at the same partial, a retry or a requeued job,
/// then measured the file before that write landed, and appended after
/// it: the stray chunk ended up in the middle of the download. Each
/// in-flight write holds the lease, so the next attempt waits for it here,
/// before [`resume_offset`] looks at the file.
pub(crate) async fn lease_part(part: &Path) -> PartLease {
    let lock = {
        let mut locks = PART_LOCKS.lock();
        locks.retain(|_, l| l.strong_count() > 0);
        match locks.get(part).and_then(std::sync::Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(part.to_path_buf(), std::sync::Arc::downgrade(&lock));
                lock
            }
        }
    };
    PartLease(std::sync::Arc::new(lock.lock_owned().await))
}

/// A download's partial, open for appending.
///
/// Writes behave as `tokio::fs::File`'s do: each is handed to a blocking
/// thread and the call returns, so the disk write overlaps the next read
/// from the network, and its error surfaces on the next call. The
/// difference is that the blocking write owns a [`PartLease`], so the
/// partial stays leased until the write lands, even when this handle and
/// the task that held it are long gone.
pub(crate) struct PartFile {
    file: std::sync::Arc<std::fs::File>,
    lease: PartLease,
    pending: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl PartFile {
    /// Wait for the write in flight, if any, and return its result.
    async fn settle(&mut self) -> std::io::Result<()> {
        match self.pending.take() {
            Some(write) => write.await.map_err(std::io::Error::other)?,
            None => Ok(()),
        }
    }

    pub(crate) async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.settle().await?;
        let (file, lease, data) = (
            std::sync::Arc::clone(&self.file),
            self.lease.clone(),
            data.to_vec(),
        );
        self.pending = Some(tokio::task::spawn_blocking(move || {
            let _lease = lease;
            std::io::Write::write_all(&mut &*file, &data)
        }));
        Ok(())
    }

    /// Wait until every write so far has reached the file.
    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.settle().await
    }

    pub(crate) async fn sync_all(&mut self) -> std::io::Result<()> {
        self.settle().await?;
        let (file, lease) = (std::sync::Arc::clone(&self.file), self.lease.clone());
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            file.sync_all()
        })
        .await
        .map_err(std::io::Error::other)?
    }
}

/// Largest sidecar [`read_sidecar`] reads. A real one is a few hundred
/// bytes; anything near this was not written by blink.
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;

/// Read a partial's sidecar: refusing to follow a symlink at `path` on Unix,
/// to read one that isn't [`ours`], or to read more than
/// [`MAX_SIDECAR_BYTES`], so one planted in a shared folder can't fill
/// memory. The open doesn't block, so a FIFO planted there can't hang the
/// download waiting for a writer; it is then refused as not a file.
async fn read_sidecar(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = opts.open(path).await?;
    // Checked on the open file, so what is read is what was checked.
    if !ours(&f.metadata().await?) {
        return Err(std::io::Error::other("not a sidecar blink wrote"));
    }
    let mut raw = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(
        &mut tokio::io::AsyncReadExt::take(f, MAX_SIDECAR_BYTES + 1),
        &mut raw,
    )
    .await?;
    if raw.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(std::io::Error::other("sidecar too large"));
    }
    Ok(raw)
}

/// Move a finished download's partial onto its final name, durably, once
/// it is checked to hold exactly the `counted` bytes the download reached
/// (its resume offset plus what it received).
///
/// The protocols check their own count; this checks the file, in case bytes
/// reached it some other way. Within blink, `lease` rules that out: no
/// earlier attempt's write can still land, and none can start before the
/// rename. This check is the backstop. A partial of any other length is
/// removed, so the next attempt starts clean, and the job fails.
///
/// The partial is already fsynced; the rename is not durable until the
/// folder holding it is synced too. Without that, a power cut could undo
/// the rename after the checkpoint had recorded the job as done, and the
/// resume would skip a file that never landed. A failed folder sync is
/// logged rather than failing the job: the file is in place, and a retry
/// would only fetch it again to the same state.
pub(crate) async fn finish_download(
    _lease: &PartLease,
    part: &Path,
    local_path: &Path,
    counted: u64,
) -> crate::error::Result<()> {
    let found = tokio::fs::symlink_metadata(part).await?;
    // Ours when opened, but in a shared folder without the sticky bit
    // another user can replace a file they don't own; what goes into place
    // must still be the user's own. See [`resume_offset`].
    if !ours(&found) {
        discard_partial(local_path).await;
        return Err(crate::error::BlinkError::transport(format!(
            "{}: the partial was replaced by a file blink didn't write",
            local_path.display()
        )));
    }
    let on_disk = found.len();
    if on_disk != counted {
        discard_partial(local_path).await;
        return Err(crate::error::BlinkError::transport(format!(
            "{}: the partial holds {on_disk} bytes, but the download reached {counted}",
            local_path.display()
        )));
    }
    tokio::fs::rename(part, local_path).await.map_err(|e| {
        crate::error::BlinkError::transport(format!("rename {}: {e}", local_path.display()))
    })?;
    let synced_path = local_path.to_path_buf();
    let synced =
        tokio::task::spawn_blocking(move || crate::paths::sync_parent_dir(&synced_path)).await;
    let failure = match synced {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(e) => Some(e.to_string()),
    };
    if let Some(error) = failure {
        tracing::warn!(
            path = %local_path.display(),
            %error,
            "could not sync the download's folder",
        );
    }
    // The partial is gone; its provenance record has nothing left to
    // describe.
    clear_part_meta(local_path).await;
    Ok(())
}

/// Drop the sidecar once the download has been renamed into place.
pub(crate) async fn clear_part_meta(local_path: &Path) {
    let _ = tokio::fs::remove_file(part_meta_path(local_path)).await;
}

/// Remove the download's partial and its sidecar, so the next attempt
/// starts from byte zero. Best-effort: a partial that is already gone is
/// what this wants.
pub(crate) async fn discard_partial(local_path: &Path) {
    let _ = remove_partial(local_path).await;
}

/// [`discard_partial`], saying whether a partial that was there is still
/// there. A partial already gone is success.
async fn remove_partial(local_path: &Path) -> std::io::Result<()> {
    let removed = match tokio::fs::remove_file(part_path(local_path)).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    clear_part_meta(local_path).await;
    removed
}

pub(crate) mod error_map;
pub mod ftp;
pub(crate) mod ftp_impl;
pub mod ftps;
/// Throwaway TLS certificates used only by `ftps`'s integration tests.
#[cfg(test)]
mod ftps_test_certs;
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

/// Maximum time allowed for `transport::open`: the TCP connect, then the SSH
/// handshake, or the FTP greeting and any TLS upgrade, then the login. Shared between the TUI initial-connect path and the dispatcher's
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
    /// Which protocol this transport speaks. Nothing reads it yet.
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

    /// Rename `from` to `to`, replacing `to` if it exists — for a rename the
    /// user confirmed as an overwrite. [`Self::rename`] may refuse an
    /// existing target, as SFTP v3 does.
    async fn rename_over(&mut self, from: &str, to: &str) -> Result<()>;

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
    /// Set only when an FTPS connect with `accept_invalid_certs = true` met
    /// a certificate the session had no pin for, and the user accepted it.
    /// The caller writes it into `session.cert_sha256`, and saves the
    /// session if [`NewCertPin::save`] says to.
    pub new_cert_pin: Option<NewCertPin>,
}

/// A certificate the user accepted on first use. See [`Connected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCertPin {
    /// Lowercase hex SHA-256 of the leaf certificate, as `cert_sha256` holds it.
    pub sha256: String,
    /// "Accept and save" rather than "trust once".
    pub save: bool,
}

/// Build the right transport for `session`. The password (if any) must be
/// resolved by the caller before this is invoked — we never store it on disk.
///
/// `app_event_tx` carries the trust prompts: the SFTP/SCP host-key one, and
/// FTPS's for an unpinned certificate.
///
/// `user_wait` is how the SSH handler marks time spent on its host-key
/// prompt, so the caller's connect deadline can exclude it; see
/// [`within_deadline_excluding_waits`]. FTPS prompts for a certificate it
/// has no pin for, when chain validation is off; plain FTP never prompts.
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
    let (transport, new_cert_pin): (Box<dyn Transport>, Option<NewCertPin>) = match session.protocol
    {
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
            let (t, pin) =
                ftps::FtpsTransport::connect(session, password, &app_event_tx, &user_wait).await?;
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
/// `name` must be a single path component: a filename from a directory
/// listing, or one the user typed. Anything else, a name containing `/`,
/// `.`, `..` or the empty name, is refused. A server could otherwise list
/// `sub/file`, or `/etc/x` (once stripped to `etc/x`), and a delete or
/// rename of the entry shown would act on another path.
///
/// Returns `None` when `name` cannot be joined safely, and the caller must
/// skip the entry. It used to return `base` unchanged in that case, which
/// callers could not tell apart from a successful join — so they acted on it.
/// The recursive delete was the sharp edge: for an entry named `..` it called
/// `remove_file` on the directory being walked rather than skipping the
/// entry.
pub(crate) fn join_remote(base: &str, name: &str) -> Option<String> {
    // `..` traverses upward; `.` is a no-op but would produce paths like
    // `/foo/./bar` that some servers don't normalise, and a server-controlled
    // `.` in a name is almost always malicious.
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
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

        /// The files this mock serves, shared, to seed or inspect.
        #[allow(dead_code)]
        pub fn files(&self) -> Arc<Mutex<HashMap<String, Vec<u8>>>> {
            Arc::clone(&self.files)
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
            // SFTP v3 semantics, as OpenSSH's server has them.
            if self.files.lock().unwrap().contains_key(to) {
                return Err(crate::error::BlinkError::transport(format!(
                    "rename {from} -> {to}: Failure"
                )));
            }
            self.rename_over(from, to).await
        }

        async fn rename_over(&mut self, from: &str, to: &str) -> Result<()> {
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
                // `/a` and what's under `/a/`, not `/ab`.
                let under = format!("{}/", remote_path.trim_end_matches('/'));
                let inside = |p: &str| p == remote_path || p.starts_with(&under);
                dirs.retain(|d| !inside(d));
                self.files.lock().unwrap().retain(|k, _| !inside(k));
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

    /// The rename checks the partial holds exactly the bytes the download
    /// counted. Both protocols checked only their own count, so bytes that
    /// reached the file some other way — the last write of a cancelled
    /// attempt landing after its retry had measured the partial — went
    /// into place as a longer, corrupt file.
    #[tokio::test]
    async fn a_partial_of_the_wrong_length_is_not_renamed_into_place() {
        let home = crate::paths::test_home();
        let local = home.path().join("f.bin");
        let part = part_path(&local);
        std::fs::write(&part, b"0123456789").unwrap();

        let lease = lease_part(&part).await;
        let err = finish_download(&lease, &part, &local, 8)
            .await
            .expect_err("10 bytes on disk for 8 counted");
        assert!(err.to_string().contains("10 bytes"), "{err}");
        assert!(!local.exists(), "nothing goes into place");
        assert!(!part.exists(), "and the partial can't be resumed from");

        std::fs::write(&part, b"01234567").unwrap();
        finish_download(&lease, &part, &local, 8).await.unwrap();
        assert_eq!(std::fs::read(&local).unwrap(), b"01234567");
    }

    /// A write handed to the blocking pool outlives the file handle that
    /// made it, and the task that held it. The next attempt at the same
    /// partial must not start until it has landed, or it measures and
    /// appends to a file that is still growing.
    ///
    /// The partial is a FIFO no one reads yet, so the first attempt's write
    /// stalls on its blocking thread for as long as the test wants.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_attempts_write_lands_before_the_next_attempt_starts() {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        use std::time::Duration;

        let home = crate::paths::test_home();
        let part = part_path(&home.path().join("f.bin"));
        let c = std::ffi::CString::new(part.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let mut reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&part)
            .unwrap();

        const LEN: usize = 1 << 20; // well past the pipe's buffer
        let lease = lease_part(&part).await;
        let mut first = open_part(&lease, &part, 1).await.unwrap();
        first.write_all(&vec![7u8; LEN]).await.unwrap();
        // As an abort or a failed attempt drops them.
        drop(first);
        drop(lease);

        let next = tokio::time::timeout(Duration::from_millis(200), lease_part(&part)).await;
        assert!(
            next.is_err(),
            "the next attempt started with a write still in flight"
        );

        let drained = tokio::task::spawn_blocking(move || {
            let (mut got, mut buf) = (0, vec![0u8; 64 * 1024]);
            while got < LEN {
                match reader.read(&mut buf) {
                    Ok(n) => got += n,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            reader
        });
        let _reader = drained.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), lease_part(&part))
            .await
            .expect("the next attempt starts once the write has landed");
    }

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

    /// The modification time the remote file has in the tests below,
    /// unless a test says otherwise.
    const MTIME: u64 = 1_790_000_000;

    fn meta(remote: &str, size: Option<u64>) -> PartMeta {
        PartMeta {
            remote_path: remote.to_string(),
            size,
            mtime: Some(MTIME),
            origin: Some(ORIGIN.to_string()),
        }
    }

    /// `decide_resume` for a download from [`ORIGIN`] of a file last
    /// modified at [`MTIME`].
    fn decide(
        part_len: Option<u64>,
        meta: Option<&PartMeta>,
        remote_path: &str,
        reported_size: Option<u64>,
    ) -> ResumeDecision {
        let now = RemoteStamp {
            size: reported_size,
            mtime: Some(MTIME),
        };
        decide_resume(part_len, meta, remote_path, now, ORIGIN)
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

    /// An unknown size on either side proves nothing about the file, so
    /// it restarts. (Before mtimes were recorded this resumed: identity was
    /// checked on path and origin alone, and a server that answers no
    /// `SIZE` could have replaced the file with anything.)
    #[test]
    fn restarts_when_the_size_is_unknown_on_either_side() {
        for (then, now) in [(None, None), (None, Some(9_000)), (Some(9_000), None)] {
            let d = decide(
                Some(4_000),
                Some(&meta("/a/report.pdf", then)),
                "/a/report.pdf",
                now,
            );
            assert_eq!(d, ResumeDecision::Fresh, "recorded {then:?}, now {now:?}");
        }
    }

    /// The case size alone missed: a file replaced by another of the same
    /// length.
    #[test]
    fn restarts_when_the_remote_file_was_modified_since_the_partial() {
        let m = PartMeta {
            mtime: Some(MTIME - 60),
            ..meta("/a/report.pdf", Some(9_000))
        };
        assert_eq!(
            decide(Some(4_000), Some(&m), "/a/report.pdf", Some(9_000)),
            ResumeDecision::Fresh,
        );
    }

    #[test]
    fn restarts_when_the_mtime_is_unknown_on_either_side() {
        let recorded = PartMeta {
            mtime: None,
            ..meta("/a/report.pdf", Some(9_000))
        };
        assert_eq!(
            decide(Some(4_000), Some(&recorded), "/a/report.pdf", Some(9_000)),
            ResumeDecision::Fresh,
            "a sidecar from before mtimes were recorded",
        );
        let now = RemoteStamp {
            size: Some(9_000),
            mtime: None,
        };
        assert_eq!(
            decide_resume(
                Some(4_000),
                Some(&meta("/a/report.pdf", Some(9_000))),
                "/a/report.pdf",
                now,
                ORIGIN,
            ),
            ResumeDecision::Fresh,
            "a server that no longer reports one",
        );
    }

    #[test]
    fn a_sidecar_without_an_mtime_still_parses() {
        let old: PartMeta = serde_json::from_str(
            r#"{"remote_path":"/a/report.pdf","size":9000,"origin":"me@files.example:22"}"#,
        )
        .unwrap();
        assert_eq!(old.mtime, None);
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

        let now = RemoteStamp {
            size: Some(10),
            mtime: Some(MTIME),
        };
        let offset = resume_offset(&local, "/r/foo", now, "origin")
            .await
            .unwrap();

        assert_eq!(offset, 0);
        assert_eq!(std::fs::read(&lookalike).unwrap(), b"a file of its own");
    }

    /// Planted in a shared folder, a partial and a sidecar naming a public
    /// file are enough to have a download resume onto someone else's bytes.
    /// Only the user's own, single-named files count.
    #[cfg(unix)]
    mod planted_partials {
        use super::*;

        fn stamp() -> RemoteStamp {
            RemoteStamp {
                size: Some(10),
                mtime: Some(MTIME),
            }
        }

        #[test]
        fn only_a_regular_file_the_user_owns_with_one_name_is_ours() {
            assert!(owned_single_file(true, 1000, 1, 1000));
            assert!(!owned_single_file(true, 1001, 1, 1000), "another user's");
            assert!(!owned_single_file(true, 1000, 2, 1000), "hard-linked");
            assert!(!owned_single_file(false, 1000, 1, 1000), "not a file");
        }

        /// The positive case the refusals below are measured against.
        #[tokio::test]
        async fn the_users_own_partial_with_its_sidecar_resumes() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            std::fs::write(part_path(&local), b"0123").unwrap();
            write_part_meta(&local, "/r/foo", stamp(), "origin").await;

            let offset = resume_offset(&local, "/r/foo", stamp(), "origin").await;

            assert_eq!(offset.unwrap(), 4);
        }

        /// Appending to a partial with a second name changes the file under
        /// that name too.
        #[tokio::test]
        async fn a_hard_linked_partial_is_not_resumed() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            let other = home.path().join("other");
            std::fs::write(&other, b"0123").unwrap();
            std::fs::hard_link(&other, part_path(&local)).unwrap();
            write_part_meta(&local, "/r/foo", stamp(), "origin").await;

            let offset = resume_offset(&local, "/r/foo", stamp(), "origin").await;

            assert_eq!(offset.unwrap(), 0);
            assert_eq!(std::fs::read(&other).unwrap(), b"0123", "left alone");
            assert!(!part_path(&local).exists(), "the partial's name is freed");
        }

        #[tokio::test]
        async fn a_hard_linked_sidecar_is_not_believed() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            std::fs::write(part_path(&local), b"0123").unwrap();
            write_part_meta(&local, "/r/foo", stamp(), "origin").await;
            std::fs::hard_link(part_meta_path(&local), home.path().join("copy")).unwrap();

            let offset = resume_offset(&local, "/r/foo", stamp(), "origin").await;

            assert_eq!(offset.unwrap(), 0);
        }

        #[tokio::test]
        async fn an_oversized_sidecar_is_not_read() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            std::fs::write(part_path(&local), b"0123").unwrap();
            write_part_meta(&local, "/r/foo", stamp(), "origin").await;
            let mut raw = std::fs::read(part_meta_path(&local)).unwrap();
            raw.resize(MAX_SIDECAR_BYTES as usize + 1, b' ');
            std::fs::write(part_meta_path(&local), &raw).unwrap();
            assert!(
                serde_json::from_slice::<PartMeta>(&raw).is_ok(),
                "trailing spaces leave it valid JSON, so only the size refuses it"
            );

            let offset = resume_offset(&local, "/r/foo", stamp(), "origin").await;

            assert_eq!(offset.unwrap(), 0);
        }

        /// Opening a FIFO for reading waits for a writer, so one planted as
        /// the sidecar used to hang the download.
        #[tokio::test]
        async fn a_fifo_planted_as_the_sidecar_doesnt_hang_the_download() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            std::fs::write(part_path(&local), b"0123").unwrap();
            let c = std::ffi::CString::new(part_meta_path(&local).as_os_str().as_encoded_bytes())
                .unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);

            let offset = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                resume_offset(&local, "/r/foo", stamp(), "origin"),
            )
            .await
            .expect("the FIFO was waited on");

            assert_eq!(offset.unwrap(), 0);
        }

        /// In `/tmp`, a partial another user left can't be removed; the
        /// download used to fail on "File exists" with nothing more said.
        #[tokio::test]
        async fn a_stale_partial_that_cant_be_removed_is_named() {
            if rustix::process::geteuid().is_root() {
                eprintln!("skipped: root can remove anything");
                return;
            }
            use std::os::unix::fs::PermissionsExt as _;
            let home = crate::paths::test_home();
            let dir = home.path().join("locked");
            std::fs::create_dir(&dir).unwrap();
            let local = dir.join("foo");
            std::fs::write(part_path(&local), b"stale").unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let result = resume_offset(&local, "/r/foo", stamp(), "origin").await;

            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let err = result.expect_err("the partial is in the way");
            let msg = err.to_string();
            assert!(msg.contains("stale partial"), "{msg}");
            assert!(msg.contains(".blink-part"), "{msg}");
        }

        /// What goes into place must still be the user's own file, not one
        /// put under the partial's name since.
        #[tokio::test]
        async fn a_partial_replaced_before_the_rename_is_not_put_in_place() {
            let home = crate::paths::test_home();
            let local = home.path().join("f.bin");
            let part = part_path(&local);
            std::fs::write(&part, b"01234567").unwrap();
            std::fs::hard_link(&part, home.path().join("second-name")).unwrap();

            let lease = lease_part(&part).await;
            let err = finish_download(&lease, &part, &local, 8)
                .await
                .expect_err("not a file blink can vouch for");

            assert!(err.to_string().contains("replaced"), "{err}");
            assert!(!local.exists());
        }
    }

    /// A partial's name is predictable, so in a shared download directory
    /// someone could plant symlinks under it ahead of the download.
    #[cfg(unix)]
    mod planted_symlinks {
        use super::*;
        use std::os::unix::fs::symlink;

        fn stamp() -> RemoteStamp {
            RemoteStamp {
                size: Some(10),
                mtime: Some(MTIME),
            }
        }

        #[tokio::test]
        async fn a_fresh_partial_is_not_created_through_a_symlink() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            let victim = home.path().join("victim");
            std::fs::write(&victim, b"precious").unwrap();
            symlink(&victim, part_path(&local)).unwrap();

            // `resume_offset` removes the link; a link re-planted after it
            // looked still can't be opened through.
            assert_eq!(
                resume_offset(&local, "/r/foo", stamp(), "origin")
                    .await
                    .unwrap(),
                0
            );
            assert!(std::fs::symlink_metadata(part_path(&local)).is_err());
            symlink(&victim, part_path(&local)).unwrap();
            let lease = lease_part(&part_path(&local)).await;
            assert!(open_part(&lease, &part_path(&local), 0).await.is_err());
            assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        }

        #[tokio::test]
        async fn a_resumed_partial_is_not_appended_through_a_symlink() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            let victim = home.path().join("victim");
            std::fs::write(&victim, b"precious").unwrap();
            symlink(&victim, part_path(&local)).unwrap();

            let lease = lease_part(&part_path(&local)).await;
            assert!(open_part(&lease, &part_path(&local), 4).await.is_err());
            assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        }

        #[tokio::test]
        async fn a_symlinked_partial_with_a_matching_sidecar_is_not_resumed() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            let victim = home.path().join("victim");
            std::fs::write(&victim, b"precious").unwrap();
            symlink(&victim, part_path(&local)).unwrap();
            write_part_meta(&local, "/r/foo", stamp(), "origin").await;

            assert_eq!(
                resume_offset(&local, "/r/foo", stamp(), "origin")
                    .await
                    .unwrap(),
                0
            );
            assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        }

        #[tokio::test]
        async fn the_sidecar_is_not_written_through_a_symlink() {
            let home = crate::paths::test_home();
            let local = home.path().join("foo");
            let victim = home.path().join("victim");
            std::fs::write(&victim, b"precious").unwrap();
            symlink(&victim, part_meta_path(&local)).unwrap();

            write_part_meta(&local, "/r/foo", stamp(), "origin").await;

            assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
            let meta = std::fs::symlink_metadata(part_meta_path(&local)).unwrap();
            assert!(meta.is_file(), "the link is replaced by a real sidecar");
        }
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

    /// The mock's recursive delete matched by prefix, so deleting `/a` took
    /// `/ab` with it, and a test relying on it could hide that bug in blink.
    #[tokio::test]
    async fn the_mocks_recursive_delete_stays_inside_the_folder() {
        let mut mock = mock::MockTransport::new()
            .with_file("/a/x", b"1")
            .with_file("/ab/y", b"2");
        mock.mkdir("/a").await.unwrap();
        mock.mkdir("/ab").await.unwrap();

        mock.delete_dir("/a", true).await.unwrap();

        assert!(mock.metadata("/a").await.unwrap().is_none());
        assert!(mock.metadata("/a/x").await.unwrap().is_none());
        assert!(mock.metadata("/ab").await.unwrap().is_some());
        assert!(mock.metadata("/ab/y").await.unwrap().is_some());
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

    /// A name with a slash is not one entry: joined, it addressed another
    /// path than the one listed, `/etc/x` once stripped to `/srv/etc/x`.
    #[test]
    fn join_rejects_a_name_with_a_slash() {
        assert_eq!(join_remote("/srv", "/etc/shadow"), None);
        assert_eq!(join_remote("/srv", "sub/file"), None);
        assert_eq!(join_remote("/srv", "file/"), None);
        assert_eq!(join_remote("/srv", "/"), None);
        assert_eq!(join_remote("/srv", ""), None);
    }

    /// Dots inside a name are just characters.
    #[test]
    fn join_keeps_names_that_merely_contain_dots() {
        assert_eq!(join_remote("/srv", "..a").as_deref(), Some("/srv/..a"));
        assert_eq!(join_remote("/srv", "...").as_deref(), Some("/srv/..."));
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
