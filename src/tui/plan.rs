//! Recursive transfer planning.
//!
//! Pure functions that walk a local or remote subtree and produce a flat
//! [`PlannedJob`] plan for the dispatcher. Conflict-detection helpers and
//! the path-safety filter live here too. None of these touch the `App`
//! struct — they take what they need as arguments and return values — so
//! they're cheap to unit-test and easy to read in isolation.
//!
//! Extracted from `app.rs` as the first step of the larger TUI split: the
//! file had become a god-object holding state, dispatch, async glue, and
//! walk planning. Walks are the cleanest cut because they share nothing
//! with the rest of the TUI machinery.

use std::path::{Path, PathBuf};

use crate::error::{BlinkError, Result};
use crate::transfer::MAX_QUEUED_JOBS;
use crate::transport::{self, EntryKind};
use crate::tui::app::SharedTransport;

// ---------------------------------------------------------------------------
// PlannedJob and WalkResult
// ---------------------------------------------------------------------------

/// One step in a recursive transfer plan, produced by [`walk_remote`] or
/// [`walk_local`] and consumed by `dispatch_plan`. The order in the produced
/// `Vec` matters: directory creations always precede the file transfers
/// inside them. The dispatcher's parallelism then takes over from there.
///
/// Files live as their own enum variant (rather than a single `Transfer` with
/// a `Direction`) so the upload/download split is type-checked at the call
/// site rather than discovered at dispatch time.
#[derive(Debug, Clone)]
pub enum PlannedJob {
    Mkdir { remote_path: String },
    Download { remote_path: String, local_path: PathBuf },
    Upload { local_path: PathBuf, remote_path: String },
}

/// Output of a recursive walk: the flat job plan plus a count of symlinks
/// that were deliberately skipped. The caller surfaces the skip count in
/// the TUI log so the user knows the plan is shorter than the tree.
#[derive(Debug)]
pub struct WalkResult {
    pub plan: Vec<PlannedJob>,
    pub symlinks_skipped: usize,
}

// ---------------------------------------------------------------------------
// Server-name safety
// ---------------------------------------------------------------------------

/// Validate a server-supplied file name before using it in a local `Path::join`.
///
/// Returns `None` if the name would escape the intended directory, alias a
/// different file, or resolve to a device. Callers must skip the entry.
///
/// Some rules only apply on Windows — a colon is a legal (if uncommon) Unix
/// filename character, and rejecting it everywhere would break legitimate
/// downloads of e.g. ISO-8601-timestamped files. See [`safe_local_name_for`]
/// for the policy split.
pub fn safe_local_name(name: &str) -> Option<&str> {
    safe_local_name_for(name, cfg!(windows))
}

/// Windows device names that resolve to hardware rather than a file,
/// regardless of the directory they appear in. Writing to `NUL` silently
/// discards the download; `COM1` opens a serial port.
const WINDOWS_RESERVED_STEMS: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The body of [`safe_local_name`], with the platform policy passed in so both
/// branches are compiled and testable on any host.
fn safe_local_name_for(name: &str, windows_rules: bool) -> Option<&str> {
    // --- Rules that apply everywhere ---
    if name.is_empty() || name == ".." || name == "." {
        return None;
    }
    if name.bytes().any(|b| matches!(b, b'\0' | b'/' | b'\\')) {
        return None;
    }

    if !windows_rules {
        return Some(name);
    }

    // --- Windows-only rules ---

    // A colon is either a drive prefix or an alternate-data-stream separator.
    // `PathBuf::push` documents that a component carrying a prefix but no root
    // replaces the entire buffer, so `base.join("C:evil")` lands outside
    // `base` altogether — a remote-controlled escape from the download tree.
    if name.contains(':') {
        return None;
    }

    // Windows strips trailing dots and spaces when resolving a path, so
    // "secret. " and "secret" name the same file. A server could use that to
    // overwrite a file the conflict check cleared under its other spelling.
    if name.ends_with('.') || name.ends_with(' ') {
        return None;
    }

    // Reserved device names match on the stem before the first dot, and are
    // case-insensitive: `NUL`, `nul.txt` and `NUL.tar.gz` are all the device.
    let stem = name.split('.').next().unwrap_or(name);
    if WINDOWS_RESERVED_STEMS
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r))
    {
        return None;
    }

    Some(name)
}

// ---------------------------------------------------------------------------
// Recursive walks
// ---------------------------------------------------------------------------

/// Walk a remote subtree rooted at `remote_root` and produce a flat plan of
/// mkdirs + file downloads with destinations rooted at `local_root`. The
/// plan is iterative-DFS pre-order so directories appear in the plan before
/// any files under them. The dispatcher will run the plan in queue order;
/// because each level's mkdir is enqueued before its file children,
/// `enqueue_mkdir` lands first by virtue of being earlier in the queue.
///
/// Transport-level errors propagate; partial trees are NOT fixed up here —
/// the caller surfaces a `WalkFailed` log line and the user can retry.
/// The transport is taken as the shared handle rather than a borrowed
/// `&mut dyn Transport`, and locked once per directory. Holding it across the
/// whole walk blocked every other user of the connection — including the pane
/// listing the UI issues on F5 or a navigation — until the walk finished,
/// which on a large tree is minutes. Every listing addresses an absolute
/// path, so interleaving another operation between two directories is safe.
pub async fn walk_remote(
    transport: &SharedTransport,
    remote_root: &str,
    local_root: &Path,
) -> Result<WalkResult> {
    let mut out: Vec<PlannedJob> = Vec::new();
    let mut symlinks_skipped: usize = 0;
    let mut dirs_visited: usize = 0;

    // Iterative DFS. Stack holds (remote_path_to_visit, local_path_dest).
    let mut stack: Vec<(String, PathBuf)> =
        vec![(remote_root.to_string(), local_root.to_path_buf())];

    while let Some((remote_dir, local_dir)) = stack.pop() {
        dirs_visited += 1;

        // Guard against pathological remote trees (or a `proc`-like FS) that
        // would otherwise OOM the walker before the dispatcher's pending-job
        // cap ever fires. Stop early with a real error so the user gets a
        // useful message instead of a process killed by the OOM killer.
        //
        // Directories have to count towards the budget, not just the jobs
        // they yield. Unlike `walk_local`, this walk emits no Mkdir job per
        // directory — only Downloads — so a remote serving a deep or wide
        // tree of *empty* directories left `out` at zero while `stack` and
        // the local mkdirs grew without limit, and the cap never fired.
        // Counting visits plus the queued stack matches the budget
        // `walk_local` gets implicitly from its per-directory Mkdir.
        if out.len() + dirs_visited + stack.len() > MAX_QUEUED_JOBS {
            return Err(BlinkError::transport(format!(
                "recursive walk exceeded {MAX_QUEUED_JOBS} entries — \
                 narrow the source or run separate transfers",
            )));
        }

        // Ensure the local dir exists ahead of file writes. (The download
        // worker also does create_dir_all on the parent, but doing it here
        // means an empty remote dir still produces a real local dir.)
        if let Err(e) = tokio::fs::create_dir_all(&local_dir).await {
            return Err(BlinkError::transport(format!(
                "create local dir {}: {e}",
                local_dir.display()
            )));
        }

        // Scoped: the guard is dropped before the entries are processed and
        // before the next iteration, so other work gets a turn.
        let entries = {
            let mut t = transport.lock().await;
            t.list(&remote_dir).await?
        };

        // Pre-collect subdirs so we can push them in reverse for a stable
        // depth-first ordering (leftmost child popped next).
        let mut subdirs: Vec<(String, PathBuf)> = Vec::new();

        for entry in entries {
            if entry.raw_name == "." || entry.raw_name == ".." {
                continue;
            }
            // Both the remote path and the local destination derive from the
            // server's own bytes. Using the sanitized form for either would
            // fetch the wrong file, or collapse two distinct entries onto one
            // local path and silently clobber one with the other.
            // `safe_local_name` is what makes the raw name safe to join —
            // it rejects separators, traversal, and the Windows hazards.
            let Some(safe_name) = safe_local_name(&entry.raw_name) else {
                tracing::warn!(
                    "skipping remote entry with unsafe name: {:?}",
                    entry.display_name
                );
                continue;
            };
            let remote_child = transport::join_remote(&remote_dir, &entry.raw_name);
            let local_child = local_dir.join(safe_name);
            match entry.kind {
                EntryKind::Directory => {
                    subdirs.push((remote_child, local_child));
                }
                EntryKind::Symlink => {
                    // Skip symlinks in recursive download. Following a
                    // server-resolved symlink would let a hostile or
                    // misconfigured remote land bytes outside the chosen
                    // destination tree (think a symlink named "passwd"
                    // pointing at /etc/passwd) or loop forever on an A→B→A
                    // cycle. Single-file View / Download of a symlink still
                    // works because the user explicitly selected it.
                    symlinks_skipped += 1;
                    tracing::debug!(
                        remote = %remote_child,
                        "skipping remote symlink in recursive download",
                    );
                }
                EntryKind::File | EntryKind::Other => {
                    // Other-kinds (sockets, devices) are rare on download
                    // targets; including them here lets the transport
                    // surface a real error rather than silently skipping
                    // data the user might have wanted.
                    out.push(PlannedJob::Download {
                        remote_path: remote_child,
                        local_path: local_child,
                    });
                }
            }
        }

        stack.extend(subdirs.into_iter().rev());
    }
    Ok(WalkResult {
        plan: out,
        symlinks_skipped,
    })
}

/// Walk a local subtree rooted at `local_root` and produce a flat plan of
/// remote mkdirs + file uploads with destinations rooted at `remote_root`.
/// Mirror image of [`walk_remote`].
pub async fn walk_local(local_root: &Path, remote_root: &str) -> Result<WalkResult> {
    let mut out: Vec<PlannedJob> = Vec::new();
    let mut symlinks_skipped: usize = 0;

    // Iterative DFS. Stack holds (local_path_to_visit, remote_path_dest).
    let mut stack: Vec<(PathBuf, String)> =
        vec![(local_root.to_path_buf(), remote_root.to_string())];

    while let Some((local_dir, remote_dir)) = stack.pop() {
        // Same cap as `walk_remote`: bail before the plan becomes unbounded.
        if out.len() > MAX_QUEUED_JOBS {
            return Err(BlinkError::transport(format!(
                "recursive walk exceeded {MAX_QUEUED_JOBS} jobs — \
                 narrow the source or run separate transfers",
            )));
        }

        // Mkdir the destination ahead of any files inside.
        out.push(PlannedJob::Mkdir {
            remote_path: remote_dir.clone(),
        });

        let mut read = tokio::fs::read_dir(&local_dir).await.map_err(|e| {
            BlinkError::transport(format!("readdir {}: {e}", local_dir.display()))
        })?;

        let mut subdirs: Vec<(PathBuf, String)> = Vec::new();

        loop {
            let next = read.next_entry().await.map_err(|e| {
                BlinkError::transport(format!("readdir entry {}: {e}", local_dir.display()))
            })?;
            let Some(entry) = next else { break };
            // file_type() reports symlinks BEFORE following them; metadata()
            // resolves them and would mask a symlink-to-dir as a regular dir.
            // Read both so we can act on the unfollowed type.
            let file_type = match entry.file_type().await {
                Ok(t) => t,
                Err(_) => continue, // unreadable entry; skip rather than fail walk
            };
            if file_type.is_symlink() {
                symlinks_skipped += 1;
                tracing::debug!(
                    local = %entry.path().display(),
                    "skipping local symlink in recursive upload",
                );
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let local_child = entry.path();
            let remote_child = transport::join_remote(&remote_dir, &name);

            if file_type.is_dir() {
                subdirs.push((local_child, remote_child));
            } else if file_type.is_file() {
                out.push(PlannedJob::Upload {
                    local_path: local_child,
                    remote_path: remote_child,
                });
            }
            // Other types (sockets, FIFOs, block/char devices) are skipped
            // silently — they have no meaningful upload payload.
        }

        stack.extend(subdirs.into_iter().rev());
    }
    Ok(WalkResult {
        plan: out,
        symlinks_skipped,
    })
}

// ---------------------------------------------------------------------------
// Conflict probes
// ---------------------------------------------------------------------------

/// Local-FS conflict probe for download plans. Iterates every Download job
/// and stat()s its destination; returns the indices whose destinations
/// already exist as files. Mkdir entries are silently merged (creating a
/// dir that exists is a no-op) and don't count as conflicts.
pub async fn find_download_conflicts(plan: &[PlannedJob]) -> Vec<usize> {
    let mut conflicts = Vec::new();
    for (i, job) in plan.iter().enumerate() {
        if let PlannedJob::Download { local_path, .. } = job
            && tokio::fs::metadata(local_path).await.is_ok() {
                conflicts.push(i);
            }
    }
    conflicts
}

/// Remote conflict probe for upload plans. Groups the plan's Upload jobs
/// by destination directory, lists each directory once, and matches names
/// in O(dirs) round-trips instead of O(files). Mkdirs aren't conflicts —
/// `transport.mkdir` is idempotent.
///
/// If a destination directory doesn't exist yet, it has no conflicts by
/// definition. We swallow the listing error in that case.
pub async fn find_upload_conflicts(
    transport: &SharedTransport,
    plan: &[PlannedJob],
) -> Result<Vec<usize>> {
    use std::collections::HashMap;

    // Group upload jobs by destination directory.
    let mut by_dir: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (i, job) in plan.iter().enumerate() {
        if let PlannedJob::Upload { remote_path, .. } = job {
            let (dir, name) = match remote_path.rsplit_once('/') {
                Some(("", n)) => ("/".to_string(), n.to_string()),
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (".".to_string(), remote_path.clone()),
            };
            by_dir.entry(dir).or_default().push((i, name));
        }
    }

    let mut conflicts = Vec::new();
    for (dir, entries) in by_dir {
        // Per-directory lock, same reasoning as `walk_remote`.
        let result = {
            let mut t = transport.lock().await;
            t.list(&dir).await
        };
        let listing = match result {
            Ok(l) => l,
            // "Directory doesn't exist yet" — no conflicts there. Other
            // errors also short-circuit to "no conflicts" because the
            // upload itself will surface the real issue with a clear path.
            Err(_) => continue,
        };
        for (i, name) in entries {
            // Compare against the server's own bytes: the upload will address
            // `name` verbatim, so the sanitized form could both miss a real
            // collision and invent one that isn't there.
            if listing.iter().any(|e| e.raw_name == name) {
                conflicts.push(i);
            }
        }
    }
    conflicts.sort_unstable();
    Ok(conflicts)
}

/// Apply the user's "skip conflicts" choice. Returns the plan with the
/// flagged jobs removed. If skipping a file makes its parent mkdir
/// unnecessary (no remaining files target that directory), we keep the
/// mkdir anyway — it's idempotent on the remote side and the cost is one
/// no-op call. Removing it would require another graph-walk over the plan
/// and the savings aren't worth the complexity.
pub fn drop_conflicting(plan: Vec<PlannedJob>, conflicts: &[usize]) -> Vec<PlannedJob> {
    use std::collections::HashSet;
    let drop: HashSet<usize> = conflicts.iter().copied().collect();
    plan.into_iter()
        .enumerate()
        .filter(|(i, _)| !drop.contains(i))
        .map(|(_, j)| j)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    // -- Walk budget -------------------------------------------------------

    /// Transport stub that reports every directory as containing `fan_out`
    /// subdirectories and no files. `walk_remote` only ever calls `list`, so
    /// nothing else needs a real implementation.
    struct EmptyDirTree {
        fan_out: usize,
    }

    #[async_trait::async_trait]
    impl Transport for EmptyDirTree {
        async fn list(&mut self, _remote_path: &str) -> Result<Vec<transport::RemoteEntry>> {
            Ok((0..self.fan_out)
                .map(|i| {
                    transport::RemoteEntry::new(
                        format!("d{i}"),
                        EntryKind::Directory,
                        0,
                        None,
                        None,
                    )
                })
                .collect())
        }

        fn protocol(&self) -> crate::session::Protocol {
            crate::session::Protocol::Sftp
        }
        async fn download(
            &mut self,
            _: &str,
            _: &Path,
            _: Option<tokio::sync::mpsc::UnboundedSender<transport::ProgressUpdate>>,
        ) -> Result<()> {
            unreachable!("walk_remote does not transfer")
        }
        async fn upload(
            &mut self,
            _: &Path,
            _: &str,
            _: Option<tokio::sync::mpsc::UnboundedSender<transport::ProgressUpdate>>,
        ) -> Result<()> {
            unreachable!("walk_remote does not transfer")
        }
        async fn rename(&mut self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn delete_file(&mut self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn delete_dir(&mut self, _: &str, _: bool) -> Result<()> {
            unreachable!()
        }
        async fn mkdir(&mut self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn metadata(&mut self, _: &str) -> Result<Option<transport::RemoteEntry>> {
            unreachable!()
        }
        async fn read_to_bytes(&mut self, _: &str) -> Result<bytes::Bytes> {
            unreachable!()
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// Wrap a stub transport in the shared handle the walk now takes.
    fn shared<T: Transport + 'static>(t: T) -> SharedTransport {
        std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(t) as Box<dyn Transport>))
    }

    /// Unique scratch directory, removed by the caller.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("blink-plan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // -- the walk must not monopolise the connection -----------------------

    /// Transport stub whose listings block until the test lets each one
    /// through, except for `/quick` which returns immediately. Lets a test
    /// hold a walk mid-flight and ask whether anything else can still use
    /// the connection.
    struct GatedTree {
        /// Sends one gate per gated `list` call; the test releases them.
        gates: tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
        fan_out: usize,
    }

    #[async_trait::async_trait]
    impl Transport for GatedTree {
        async fn list(&mut self, remote_path: &str) -> Result<Vec<transport::RemoteEntry>> {
            if remote_path != "/quick" {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = self.gates.send(tx);
                let _ = rx.await;
            }
            // Only the root fans out; children are leaves, so the walk makes
            // exactly `fan_out + 1` gated listings.
            let n = if remote_path == "/gated" { self.fan_out } else { 0 };
            Ok((0..n)
                .map(|i| {
                    transport::RemoteEntry::new(
                        format!("d{i}"),
                        EntryKind::Directory,
                        0,
                        None,
                        None,
                    )
                })
                .collect())
        }

        fn protocol(&self) -> crate::session::Protocol {
            crate::session::Protocol::Sftp
        }
        async fn download(
            &mut self,
            _: &str,
            _: &Path,
            _: Option<tokio::sync::mpsc::UnboundedSender<transport::ProgressUpdate>>,
        ) -> Result<()> {
            unreachable!()
        }
        async fn upload(
            &mut self,
            _: &Path,
            _: &str,
            _: Option<tokio::sync::mpsc::UnboundedSender<transport::ProgressUpdate>>,
        ) -> Result<()> {
            unreachable!()
        }
        async fn rename(&mut self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn delete_file(&mut self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn delete_dir(&mut self, _: &str, _: bool) -> Result<()> {
            unreachable!()
        }
        async fn mkdir(&mut self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn metadata(&mut self, _: &str) -> Result<Option<transport::RemoteEntry>> {
            unreachable!()
        }
        async fn read_to_bytes(&mut self, _: &str) -> Result<bytes::Bytes> {
            unreachable!()
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// The walk must release the connection between directories.
    ///
    /// It used to hold the transport lock for the whole tree, so a listing
    /// requested from the UI — F5, or navigating — waited for the entire
    /// walk to finish. On a large remote that is minutes of a pane that has
    /// already been blanked.
    ///
    /// Deterministic rather than timing-based: exactly one gated listing is
    /// released, so the walk is parked inside its *second* one. If the lock
    /// were held across the walk, the pane listing could never complete —
    /// the walk is not going to finish.
    #[tokio::test]
    async fn a_walk_releases_the_connection_between_directories() {
        let root = scratch("gated");
        let (gate_tx, mut gate_rx) = tokio::sync::mpsc::unbounded_channel();
        let shared: SharedTransport = std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(
            GatedTree {
                gates: gate_tx,
                fan_out: 3,
            },
        )
            as Box<dyn Transport>));

        let walk_handle = {
            let shared = shared.clone();
            let root = root.clone();
            tokio::spawn(async move { walk_remote(&shared, "/gated", &root).await.map(|_| ()) })
        };

        // Wait until the walk is inside its first listing, then let just that
        // one through. The walk then parks on the next one.
        let first = gate_rx.recv().await.expect("walk should start listing");
        let _ = first.send(());

        // A pane refresh, issued while the walk is still in flight.
        let listed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut t = shared.lock().await;
            t.list("/quick").await
        })
        .await;

        assert!(
            listed.is_ok(),
            "a pane listing must not wait for the whole walk to finish",
        );

        // Let the walk drain so the task doesn't outlive the test.
        while let Ok(gate) = gate_rx.try_recv() {
            let _ = gate.send(());
        }
        walk_handle.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn walk_remote_counts_directories_towards_the_cap() {
        // The regression: `walk_remote` emits Downloads only, never a Mkdir,
        // so a tree of empty directories left `out` at zero and the cap never
        // fired no matter how much the walk expanded. A fan-out wider than
        // the budget must now be refused.
        let root = scratch("wide");
        let t = EmptyDirTree {
            fan_out: MAX_QUEUED_JOBS + 1,
        };

        let err = walk_remote(&shared(t), "/", &root)
            .await
            .expect_err("a tree this wide must not be planned");
        assert!(
            err.to_string().contains("exceeded"),
            "expected the walk budget error, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn walk_remote_still_accepts_a_small_empty_tree() {
        // The guard must not fire on ordinary input — an empty directory
        // tree is legitimate, it just plans no transfers.
        let root = scratch("small");
        let t = EmptyDirTree { fan_out: 0 };

        let result = walk_remote(&shared(t), "/", &root).await.expect("must succeed");
        assert!(result.plan.is_empty(), "no files means no jobs");
        assert_eq!(result.symlinks_skipped, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    // -- raw vs. display names ---------------------------------------------
    //
    // `list()` sanitizes names for terminal rendering: control and bidi
    // characters become spaces, and the string is truncated. That is correct
    // for what the user *sees* and wrong for what blink *addresses* — the
    // sanitized form names a different file, or no file at all. These pin the
    // rule that everything path-building reads the server's own bytes.

    /// Transport stub whose root directory holds exactly the given files.
    struct FileList {
        names: Vec<String>,
    }

    #[async_trait::async_trait]
    impl Transport for FileList {
        async fn list(&mut self, _remote_path: &str) -> Result<Vec<transport::RemoteEntry>> {
            Ok(self
                .names
                .iter()
                .map(|n| {
                    transport::RemoteEntry::new(
                        n.clone(),
                        EntryKind::File,
                        3,
                        None,
                        None,
                    )
                })
                .collect())
        }

        fn protocol(&self) -> crate::session::Protocol {
            crate::session::Protocol::Sftp
        }
        async fn download(
            &mut self,
            _: &str,
            _: &Path,
            _: Option<tokio::sync::mpsc::UnboundedSender<transport::ProgressUpdate>>,
        ) -> Result<()> {
            unreachable!()
        }
        async fn upload(
            &mut self,
            _: &Path,
            _: &str,
            _: Option<tokio::sync::mpsc::UnboundedSender<transport::ProgressUpdate>>,
        ) -> Result<()> {
            unreachable!()
        }
        async fn rename(&mut self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn delete_file(&mut self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn delete_dir(&mut self, _: &str, _: bool) -> Result<()> {
            unreachable!()
        }
        async fn mkdir(&mut self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn metadata(&mut self, _: &str) -> Result<Option<transport::RemoteEntry>> {
            unreachable!()
        }
        async fn read_to_bytes(&mut self, _: &str) -> Result<bytes::Bytes> {
            unreachable!()
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn walk_remote_addresses_files_by_the_name_the_server_sent() {
        // A right-to-left override in the name sanitizes to a space. Fetching
        // "/srv/re port.txt" asks for a file that does not exist.
        let root = scratch("rawname");
        let raw = "re\u{202E}port.txt";
        let t = FileList {
            names: vec![raw.to_string()],
        };

        let result = walk_remote(&shared(t), "/srv", &root).await.expect("walk");

        match result.plan.as_slice() {
            [PlannedJob::Download { remote_path, .. }] => assert_eq!(
                remote_path,
                &format!("/srv/{raw}"),
                "the remote path must carry the server's own bytes",
            ),
            other => panic!("expected one Download, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn walk_remote_keeps_names_that_sanitize_alike_distinct() {
        // Both names render identically once sanitized. If the plan is built
        // from the rendered form, one job addresses the other's file — and
        // both land on one local path.
        let root = scratch("collide");
        let decoy = "invoice\u{200B}.pdf";
        let real = "invoice .pdf";
        let t = FileList {
            names: vec![decoy.to_string(), real.to_string()],
        };

        let result = walk_remote(&shared(t), "/srv", &root).await.expect("walk");

        let mut remotes: Vec<&str> = result
            .plan
            .iter()
            .map(|j| match j {
                PlannedJob::Download { remote_path, .. } => remote_path.as_str(),
                other => panic!("expected Downloads, got {other:?}"),
            })
            .collect();
        remotes.sort_unstable();
        assert_eq!(remotes.len(), 2);
        assert_ne!(
            remotes[0], remotes[1],
            "two distinct server names must stay two distinct remote paths",
        );

        let mut locals: Vec<PathBuf> = result
            .plan
            .iter()
            .map(|j| match j {
                PlannedJob::Download { local_path, .. } => local_path.clone(),
                other => panic!("expected Downloads, got {other:?}"),
            })
            .collect();
        locals.sort();
        assert_ne!(
            locals[0], locals[1],
            "collapsing them locally would silently clobber one file with the other",
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_entries_sanitize_the_display_name_only() {
        let e = transport::RemoteEntry::new(
            "re\u{202E}port.txt".to_string(),
            EntryKind::File,
            1,
            None,
            None,
        );
        assert_eq!(e.raw_name, "re\u{202E}port.txt", "wire name is verbatim");
        assert_eq!(e.display_name, "re port.txt", "rendered name is sanitized");
    }

    #[test]
    fn safe_local_name_rejects_traversal_and_separators() {
        assert!(safe_local_name("..").is_none());
        assert!(safe_local_name(".").is_none());
        assert!(safe_local_name("").is_none());
        assert!(safe_local_name("a/b").is_none());
        assert!(safe_local_name("a\\b").is_none());
        assert!(safe_local_name("a\0b").is_none());
        assert_eq!(safe_local_name("ok.txt"), Some("ok.txt"));
    }

    // -- Windows name rules ------------------------------------------------
    //
    // These exercise `safe_local_name_for(.., true)` directly so the Windows
    // policy is covered when running the suite on any host, not only Windows.

    #[test]
    fn windows_rejects_drive_prefix() {
        // `PathBuf::push` documents that a component with a prefix but no
        // root REPLACES the whole buffer on Windows, so `base.join("C:evil")`
        // escapes `base` entirely.
        assert!(safe_local_name_for("C:evil.txt", true).is_none());
        assert!(safe_local_name_for("c:", true).is_none());
    }

    #[test]
    fn windows_rejects_alternate_data_stream() {
        assert!(safe_local_name_for("report.txt:hidden", true).is_none());
    }

    #[test]
    fn windows_rejects_trailing_dot_or_space() {
        // Windows silently strips these, so "secret. " aliases "secret".
        assert!(safe_local_name_for("secret.", true).is_none());
        assert!(safe_local_name_for("secret ", true).is_none());
        assert!(safe_local_name_for("secret. ", true).is_none());
    }

    #[test]
    fn windows_rejects_reserved_device_names() {
        for name in ["NUL", "nul", "CON", "aux", "COM1", "lpt9", "NUL.txt", "con.tar.gz"] {
            assert!(
                safe_local_name_for(name, true).is_none(),
                "expected {name:?} to be rejected on Windows"
            );
        }
    }

    #[test]
    fn windows_allows_names_merely_resembling_devices() {
        for name in ["CONSOLE", "COM", "COM10", "NULL", "lpt", "nul_backup"] {
            assert_eq!(
                safe_local_name_for(name, true),
                Some(name),
                "expected {name:?} to be allowed on Windows"
            );
        }
    }

    #[test]
    fn unix_still_allows_colons_and_device_names() {
        // A colon is a legal, occasionally-used Unix filename character
        // (timestamps in particular). The Windows rules must not leak over.
        assert_eq!(
            safe_local_name_for("2024-01-01T00:00:00.log", false),
            Some("2024-01-01T00:00:00.log")
        );
        assert_eq!(safe_local_name_for("NUL", false), Some("NUL"));
        assert_eq!(safe_local_name_for("trailing.", false), Some("trailing."));
    }

    #[test]
    fn universal_rules_apply_under_windows_too() {
        assert!(safe_local_name_for("..", true).is_none());
        assert!(safe_local_name_for("a/b", true).is_none());
        assert!(safe_local_name_for("a\0b", true).is_none());
        assert_eq!(safe_local_name_for("ok.txt", true), Some("ok.txt"));
    }

    #[test]
    fn drop_conflicting_removes_only_listed_indices() {
        let plan = vec![
            PlannedJob::Mkdir { remote_path: "/a".into() },
            PlannedJob::Upload {
                local_path: PathBuf::from("/local/a.txt"),
                remote_path: "/a/a.txt".into(),
            },
            PlannedJob::Upload {
                local_path: PathBuf::from("/local/b.txt"),
                remote_path: "/a/b.txt".into(),
            },
        ];
        let out = drop_conflicting(plan, &[1]);
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], PlannedJob::Mkdir { remote_path } if remote_path == "/a"));
        assert!(matches!(&out[1], PlannedJob::Upload { remote_path, .. } if remote_path == "/a/b.txt"));
    }

    #[test]
    fn drop_conflicting_keeps_mkdirs_even_when_all_files_skipped() {
        // The mkdir for /a is at index 0; the only file under it is index 1.
        // Skipping the file should NOT drop the mkdir — `transport.mkdir`
        // is idempotent, and recalculating which mkdirs are still needed
        // would require another graph walk.
        let plan = vec![
            PlannedJob::Mkdir { remote_path: "/a".into() },
            PlannedJob::Upload {
                local_path: PathBuf::from("/local/x"),
                remote_path: "/a/x".into(),
            },
        ];
        let out = drop_conflicting(plan, &[1]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], PlannedJob::Mkdir { .. }));
    }
}
