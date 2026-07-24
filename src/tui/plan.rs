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
use crate::transport::{self, EntryKind, Transport};

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
pub async fn walk_remote(
    transport: &mut dyn Transport,
    remote_root: &str,
    local_root: &Path,
) -> Result<WalkResult> {
    let mut out: Vec<PlannedJob> = Vec::new();
    let mut symlinks_skipped: usize = 0;

    // Iterative DFS. Stack holds (remote_path_to_visit, local_path_dest).
    let mut stack: Vec<(String, PathBuf)> =
        vec![(remote_root.to_string(), local_root.to_path_buf())];

    while let Some((remote_dir, local_dir)) = stack.pop() {
        // Guard against pathological remote trees (or a `proc`-like FS) that
        // would otherwise OOM the walker before the dispatcher's pending-job
        // cap ever fires. Stop early with a real error so the user gets a
        // useful message instead of a process killed by the OOM killer.
        if out.len() > MAX_QUEUED_JOBS {
            return Err(BlinkError::transport(format!(
                "recursive walk exceeded {MAX_QUEUED_JOBS} jobs — \
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

        let entries = transport.list(&remote_dir).await?;

        // Pre-collect subdirs so we can push them in reverse for a stable
        // depth-first ordering (leftmost child popped next).
        let mut subdirs: Vec<(String, PathBuf)> = Vec::new();

        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let Some(safe_name) = safe_local_name(&entry.name) else {
                tracing::warn!("skipping remote entry with unsafe name: {:?}", entry.name);
                continue;
            };
            let remote_child = transport::join_remote(&remote_dir, &entry.name);
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
    transport: &mut dyn Transport,
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
        let listing = match transport.list(&dir).await {
            Ok(l) => l,
            // "Directory doesn't exist yet" — no conflicts there. Other
            // errors also short-circuit to "no conflicts" because the
            // upload itself will surface the real issue with a clear path.
            Err(_) => continue,
        };
        for (i, name) in entries {
            if listing.iter().any(|e| e.name == name) {
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
