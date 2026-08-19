# Checkpoint Resume Offer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Status:** Implemented and shipped in 0.6.0; all coded steps verified against the tree on 2026-08-19 (`cargo test`: 366 passed). The Manual Verification Checklist at the end is unticked because it needs a live server and an interactive run.

**Goal:** Offer to resume an interrupted transfer batch when connecting to a session that has a checkpoint on disk, via a post-connect panel with a short summary.

**Architecture:** Discovery (`checkpoint::offers_for`) produces display-only summaries. A `VecDeque<PostConnectOffer>` on `App` owns post-connect sequencing — checkpoints first, then the existing save-session offer. One panel per checkpoint, shown in turn. Resume delegates to the existing `App::resume_walk`; discard sweeps orphaned `.part` files and removes the file.

**Tech Stack:** Rust 2024, ratatui 0.29, tokio, serde_json. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-08-checkpoint-resume-offer-design.md`

## Global Constraints

- Every task ends green: `cargo test`, and `cargo clippy --all-targets -- -D warnings` clean.
- TDD: write the failing test, watch it fail for the right reason, then implement.
- If a test compiles straight to green (common in Rust when adding a type), re-break the implementation to confirm the test catches it before moving on.
- Tests must not leave files in the user's real `~/.config/blink/checkpoints/`.
- Remote paths carry the server's own bytes. Anything rendered must be sanitized — via `push_log` (which sanitizes centrally) or `error::sanitize` at construction.
- Commit after each task. Do not push.

## File Map

| File | Responsibility |
|---|---|
| `src/checkpoint.rs` | `CheckpointOffer`, `offers_for`, `discard`, `DiscardOutcome`; `remove_orphan_parts` returns instead of printing; `test_support::CheckpointCleanup` |
| `src/tui/state.rs` | `PostConnectOffer` enum |
| `src/tui/app/mod.rs` | `Screen::OfferResumeCheckpoint`, `pending_offers`, `show_next_offer`, `draw`/`handle_key` arms, `disconnect` fix |
| `src/tui/app/handlers.rs` | `handle_offer_resume_checkpoint`; `handle_offer_save_session` pops and advances |
| `src/tui/app/events.rs` | `Connected` builds the offer queue |
| `src/tui/views.rs` | `offer_resume_checkpoint` render module |
| `README.md` | panel docs, name-keying note, stale reference fix |

---

## Task 1: Panic-safe cleanup for checkpoint tests

Checkpoint tests write to the user's real `~/.config/blink/checkpoints/`, because `Checkpoint::path_for` resolves through `paths::checkpoints_dir()` and there is no injection point. The existing App-level tests clean up on their last line, which means a panicking test leaves a `blink-test-*` file behind — and a failing test is exactly when that happens.

A `Drop` guard fixes it without threading a directory through every call site. Every later task in this plan writes checkpoints from tests, so this comes first.

**Files:**
- Modify: `src/checkpoint.rs` (new `#[cfg(test)] mod test_support`)
- Modify: `src/tui/app/mod.rs` (`checkpoint_app` returns the guard; existing callers updated)

**Interfaces:**
- Consumes: nothing.
- Produces: `checkpoint::test_support::CheckpointCleanup::new(session: impl Into<String>) -> CheckpointCleanup`, which removes both of a session's checkpoints when dropped.

- [x] **Step 1: Write the failing test**

Add to `src/checkpoint.rs`, before `mod merge_tests`:

```rust
#[cfg(test)]
pub(crate) mod test_support {
    use super::{Checkpoint, CheckpointKind};

    /// Removes a test session's checkpoints when it drops.
    ///
    /// Tests resolve through the user's real checkpoint directory — there is
    /// no injection point on `paths::checkpoints_dir` — so cleanup on the
    /// last line of a test leaves a file behind whenever that test panics,
    /// which is precisely when it matters.
    pub struct CheckpointCleanup(String);

    impl CheckpointCleanup {
        pub fn new(session: impl Into<String>) -> Self {
            Self(session.into())
        }

        pub fn session(&self) -> &str {
            &self.0
        }
    }

    impl Drop for CheckpointCleanup {
        fn drop(&mut self) {
            for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
                let _ = Checkpoint::remove(&self.0, kind);
            }
        }
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::test_support::CheckpointCleanup;
    use super::*;

    #[test]
    fn the_guard_removes_the_checkpoint_when_it_drops() {
        let name = format!("blink-test-guard-{}", std::process::id());
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();
        {
            let _guard = CheckpointCleanup::new(&name);
            let mut cp = Checkpoint::new(&name, CheckpointKind::Download, Vec::new());
            cp.flush().expect("write the checkpoint");
            assert!(path.exists(), "fixture must have written something");
        }
        assert!(!path.exists(), "the guard must clean up on drop");
    }

    #[test]
    fn the_guard_cleans_up_even_when_a_test_panics() {
        let name = format!("blink-test-panic-{}", std::process::id());
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();

        let name_inner = name.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = CheckpointCleanup::new(&name_inner);
            let mut cp = Checkpoint::new(&name_inner, CheckpointKind::Download, Vec::new());
            cp.flush().expect("write the checkpoint");
            panic!("as a failing test would");
        });

        assert!(result.is_err(), "the closure must have panicked");
        assert!(!path.exists(), "unwinding must still run the guard");
    }
}
```

`Checkpoint::path_for` is private. Change its signature to `pub(crate) fn path_for` — Task 4 needs it too.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet cleanup_tests`
Expected: FAIL to compile — `cannot find module test_support`, `path_for` is private.

- [x] **Step 3: Implement**

The module above *is* the implementation — it is test-only code. Add it, make `path_for` `pub(crate)`, and re-run.

- [x] **Step 4: Adopt it in the existing App tests**

In `src/tui/app/mod.rs`, change `checkpoint_app` to hand back a guard alongside the app, and delete `clean_checkpoints`:

```rust
    /// An app whose checkpoints go under a name no real session will use,
    /// plus a guard that removes them however the test ends.
    fn checkpoint_app(tag: &str) -> (App, crate::checkpoint::test_support::CheckpointCleanup) {
        let mut a = app();
        let name = format!("blink-test-{tag}-{}", std::process::id());
        let mut s = Session::from_url("sftp://me@host").unwrap();
        s.name = name.clone();
        a.current_session = Some(s);
        a.transfer_manager = Some(TransferManager::new(1).0);
        (a, crate::checkpoint::test_support::CheckpointCleanup::new(name))
    }
```

Update every existing caller — `a_second_batch_joins_the_first_instead_of_replacing_it`, `upload_and_download_batches_keep_separate_checkpoints`, `cancelling_one_batch_leaves_the_other_direction_alone`, `completing_a_job_coalesces_its_mark_instead_of_writing`, `cancelling_a_batch_writes_immediately` — from:

```rust
        let mut a = checkpoint_app("merge");
        // …
        clean_checkpoints(&a);
```

to:

```rust
        let (mut a, _cleanup) = checkpoint_app("merge");
        // …  (drop the trailing clean_checkpoints call)
```

- [x] **Step 5: Run tests and confirm nothing is left behind**

Run:
```bash
cargo test --quiet && cargo clippy --all-targets -- -D warnings
ls ~/.config/blink/checkpoints/ | grep blink-test && echo "STRAY FILES" || echo "clean"
```
Expected: all pass, clippy silent, `clean`.

- [x] **Step 6: Commit**

```bash
git add src/checkpoint.rs src/tui/app/mod.rs
git commit -m "test(checkpoint): clean up test checkpoints on panic too

Checkpoint tests resolve through the user's real checkpoint directory —
paths::checkpoints_dir has no injection point — so cleaning up on the last
line of a test leaves a file behind whenever that test panics, which is
exactly when it happens. A Drop guard runs during unwinding."
```

---

## Task 2: Clear checkpoint state on disconnect

Prerequisite from the spec. `disconnect` leaves `active_checkpoints` and `checkpoint_job_map` populated. Two failures follow: the new panel would offer a resume `resume_walk` then refuses, and — independent of this feature — a new `TransferManager` restarts job ids at 1, so a fresh job's id can collide with a stale map entry and mark the wrong checkpoint entry done.

**Files:**
- Modify: `src/tui/app/mod.rs` (`disconnect`, and its test module)

**Interfaces:**
- Consumes: nothing.
- Produces: nothing new. Later tasks rely on `disconnect` leaving `active_checkpoints` empty.

- [x] **Step 1: Write the failing test**

Add to the `tests` module in `src/tui/app/mod.rs`, next to the other checkpoint tests:

```rust
#[tokio::test]
async fn disconnecting_clears_checkpoint_state() {
    // A new connection gets a fresh TransferManager whose job ids restart
    // at 1. Leaving the old id map in place lets a new job's id collide
    // with a stale entry and mark the wrong checkpoint entry done.
    let (mut a, _cleanup) = checkpoint_app("disconnect");
    a.dispatch_plan(vec![download(0), download(1)], Direction::Download);
    assert!(!a.active_checkpoints.is_empty(), "fixture must set up state");
    assert!(!a.checkpoint_job_map.is_empty());

    a.disconnect();

    assert!(
        a.active_checkpoints.is_empty(),
        "a checkpoint from the previous connection must not survive it",
    );
    assert!(a.checkpoint_job_map.is_empty(), "stale job ids must not survive");
}
```

The guard owns cleanup, which matters here: `disconnect` clears `current_session`, so a test that cleaned up from `a` afterwards would have nothing to clean up *by*.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet disconnecting_clears_checkpoint_state`
Expected: FAIL — `a checkpoint from the previous connection must not survive it`

- [x] **Step 3: Implement**

In `src/tui/app/mod.rs`, `disconnect`, in the "Clear connection-scoped state" block alongside `self.transfer_manager = None;`:

```rust
        // Checkpoint bookkeeping is scoped to the connection that created
        // it. The next connection gets a fresh TransferManager whose job
        // ids restart at 1, so a surviving map would alias new jobs onto
        // old checkpoint entries — and a surviving checkpoint would make
        // `resume_walk` refuse a resume the user was just offered.
        self.active_checkpoints.clear();
        self.checkpoint_job_map.clear();
```

- [x] **Step 4: Run tests**

Run: `cargo test --quiet && cargo clippy --all-targets -- -D warnings`
Expected: all pass, clippy silent.

- [x] **Step 5: Commit**

```bash
git add src/tui/app/mod.rs
git commit -m "fix(tui): clear checkpoint state on disconnect

A new connection gets a fresh TransferManager whose job ids restart at 1,
so a surviving checkpoint_job_map aliases new jobs onto old checkpoint
entries and marks the wrong one done. A surviving active checkpoint also
makes resume_walk refuse a resume that is legitimately available."
```

---

## Task 3: `remove_orphan_parts` returns instead of printing

`remove_orphan_parts` reports failures with `eprintln!`. That is right for the CLI and wrong for the TUI, where writing to stderr smears the display.

**Files:**
- Modify: `src/checkpoint.rs` (`remove_orphan_parts`, `list_and_clean`, tests)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct DiscardOutcome { pub parts_removed: usize, pub failures: Vec<String> }`; `fn remove_orphan_parts(cp: &Checkpoint) -> DiscardOutcome`.

- [x] **Step 1: Write the failing test**

Add a new module in `src/checkpoint.rs`, before `mod merge_tests`:

```rust
#[cfg(test)]
mod sweep_tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch directory holding real `.part` files for the sweep to find.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("blink-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn job(dir: &Path, name: &str, status: JobStatus) -> CheckpointJob {
        CheckpointJob::Download {
            remote_path: format!("/r/{name}"),
            local_path: dir.join(name),
            status,
        }
    }

    #[test]
    fn the_sweep_reports_what_it_removed() {
        let dir = scratch("reports");
        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let finished = job(&dir, "b.bin", JobStatus::Done);
        // Both have a partial on disk; only the unfinished one is orphaned.
        for j in [&unfinished, &finished] {
            let CheckpointJob::Download { local_path, .. } = j else { unreachable!() };
            std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();
        }
        let cp = Checkpoint::new("s", CheckpointKind::Download, vec![unfinished, finished]);

        let outcome = remove_orphan_parts(&cp);

        assert_eq!(outcome.parts_removed, 1, "only the unfinished job's partial");
        assert!(outcome.failures.is_empty());
        assert!(
            std::fs::metadata(crate::transport::part_path(&dir.join("b.bin"))).is_ok(),
            "a completed job's partial belongs to some other transfer",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_partial_is_not_a_failure() {
        let dir = scratch("missing");
        let cp = Checkpoint::new(
            "s",
            CheckpointKind::Download,
            vec![job(&dir, "never-started.bin", JobStatus::Pending)],
        );

        let outcome = remove_orphan_parts(&cp);

        assert_eq!(outcome.parts_removed, 0);
        assert!(outcome.failures.is_empty(), "the job simply never started");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet sweep_tests`
Expected: FAIL to compile — `cannot find type DiscardOutcome`, and `remove_orphan_parts` returns `usize` not a struct.

- [x] **Step 3: Implement**

In `src/checkpoint.rs`, replace `remove_orphan_parts` and add the struct above it:

```rust
/// What a checkpoint teardown removed, and what it could not.
///
/// Returned rather than printed: the CLI writes failures to stderr, but the
/// TUI has to route them through its log — writing to stderr under a
/// full-screen terminal UI smears the display.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DiscardOutcome {
    pub parts_removed: usize,
    pub failures: Vec<String>,
}

fn remove_orphan_parts(cp: &Checkpoint) -> DiscardOutcome {
    let mut outcome = DiscardOutcome::default();
    for job in &cp.jobs {
        let CheckpointJob::Download { local_path, status, .. } = job else {
            continue;
        };
        if *status == JobStatus::Done {
            continue;
        }
        let part = crate::transport::part_path(local_path);
        match std::fs::remove_file(&part) {
            Ok(()) => outcome.parts_removed += 1,
            // Not there is the normal case — the job may never have started.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => outcome
                .failures
                .push(format!("could not remove {}: {e}", part.display())),
        }
    }
    outcome
}
```

Update the call site in `list_and_clean`, which currently reads `parts_removed += remove_orphan_parts(&cp);`:

```rust
            let swept = remove_orphan_parts(&cp);
            parts_removed += swept.parts_removed;
            for failure in swept.failures {
                eprintln!("warning: {failure}");
            }
```

- [x] **Step 4: Run tests**

Run: `cargo test --quiet && cargo clippy --all-targets -- -D warnings`
Expected: all pass, clippy silent.

- [x] **Step 5: Commit**

```bash
git add src/checkpoint.rs
git commit -m "refactor(checkpoint): return sweep results instead of printing them

remove_orphan_parts reported failures with eprintln!, which is right for
the CLI and wrong for the TUI — writing to stderr under a full-screen
terminal UI smears the display. It now returns a DiscardOutcome; the CLI
prints, and the resume panel will log."
```

---

## Task 4: `CheckpointOffer` and `offers_for`

**Files:**
- Modify: `src/checkpoint.rs`

**Interfaces:**
- Consumes: `DiscardOutcome` (Task 3) — not directly, but the same module.
- Produces:
  - `pub struct CheckpointOffer { pub kind: CheckpointKind, pub session: String, pub remaining: usize, pub total: usize, pub age: Option<Duration>, pub sample_paths: Vec<String> }`
  - `pub fn offers_for(session: &str) -> Vec<CheckpointOffer>`

- [x] **Step 1: Write the failing test**

Add to `src/checkpoint.rs`, before `mod merge_tests`:

```rust
#[cfg(test)]
mod offer_tests {
    use super::*;
    use std::path::PathBuf;

    fn dl(n: usize, status: JobStatus) -> CheckpointJob {
        CheckpointJob::Download {
            remote_path: format!("/srv/file{n}.bin"),
            local_path: PathBuf::from(format!("/l/file{n}.bin")),
            status,
        }
    }

    #[test]
    fn the_offer_counts_only_outstanding_and_completed_work() {
        // `total` is remaining + done. A cancelled job is work the user
        // already abandoned; counting it would overstate what is left.
        let cp = Checkpoint::new(
            "s",
            CheckpointKind::Download,
            vec![
                dl(0, JobStatus::Done),
                dl(1, JobStatus::Pending),
                dl(2, JobStatus::InProgress),
                dl(3, JobStatus::Cancelled),
            ],
        );

        let offer = cp.to_offer(None);

        assert_eq!(offer.remaining, 2, "pending + in_progress");
        assert_eq!(offer.total, 3, "remaining + done, cancelled excluded");
        assert_eq!(offer.kind, CheckpointKind::Download);
    }

    #[test]
    fn the_offer_samples_at_most_three_outstanding_paths() {
        let jobs: Vec<CheckpointJob> = (0..10).map(|n| dl(n, JobStatus::Pending)).collect();
        let cp = Checkpoint::new("s", CheckpointKind::Download, jobs);

        let offer = cp.to_offer(None);

        assert_eq!(offer.sample_paths.len(), 3);
        assert_eq!(offer.sample_paths[0], "/srv/file0.bin");
    }

    #[test]
    fn the_offer_skips_finished_jobs_when_sampling() {
        let cp = Checkpoint::new(
            "s",
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done), dl(1, JobStatus::Pending)],
        );

        let offer = cp.to_offer(None);

        assert_eq!(
            offer.sample_paths,
            vec!["/srv/file1.bin".to_string()],
            "the panel should show what is left, not what is finished",
        );
    }

    #[test]
    fn an_upload_offer_samples_the_local_source() {
        // For an upload the user picked local files; that is what they will
        // recognise, not the remote destination.
        let cp = Checkpoint::new(
            "s",
            CheckpointKind::Upload,
            vec![CheckpointJob::Upload {
                local_path: PathBuf::from("/home/me/photos/a.cr2"),
                remote_path: "/srv/backup/a.cr2".into(),
                status: JobStatus::Pending,
            }],
        );

        let offer = cp.to_offer(None);

        assert_eq!(offer.sample_paths, vec!["/home/me/photos/a.cr2".to_string()]);
    }

    #[test]
    fn sample_paths_are_sanitized() {
        // The panel renders these directly rather than through push_log, so
        // a server-supplied name must not carry escapes into the terminal.
        let cp = Checkpoint::new(
            "s",
            CheckpointKind::Download,
            vec![CheckpointJob::Download {
                remote_path: "/srv/re\u{202E}port.bin".into(),
                local_path: PathBuf::from("/l/x"),
                status: JobStatus::Pending,
            }],
        );

        let offer = cp.to_offer(None);

        assert!(
            !offer.sample_paths[0].contains('\u{202E}'),
            "bidi override reached the panel: {:?}",
            offer.sample_paths[0],
        );
    }

    #[test]
    fn a_session_with_no_checkpoints_has_no_offers() {
        let name = format!("blink-test-none-{}", std::process::id());
        assert!(offers_for(&name).is_empty());
    }

    #[test]
    fn a_pending_checkpoint_on_disk_produces_one_offer() {
        let name = format!("blink-test-offer-{}", std::process::id());
        let _cleanup = test_support::CheckpointCleanup::new(&name);
        let mut cp = Checkpoint::new(
            &name,
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done), dl(1, JobStatus::Pending)],
        );
        cp.flush().expect("write the checkpoint");

        let offers = offers_for(&name);

        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].remaining, 1);
        assert_eq!(offers[0].total, 2);
        assert!(offers[0].age.is_some(), "age comes from the file mtime");
    }

    #[test]
    fn a_finished_checkpoint_produces_no_offer() {
        let name = format!("blink-test-done-{}", std::process::id());
        let _cleanup = test_support::CheckpointCleanup::new(&name);
        let mut cp = Checkpoint::new(
            &name,
            CheckpointKind::Download,
            vec![dl(0, JobStatus::Done)],
        );
        cp.flush().expect("write the checkpoint");

        assert!(
            offers_for(&name).is_empty(),
            "nothing left to resume means nothing to offer",
        );
    }

    #[test]
    fn both_directions_each_produce_an_offer() {
        let name = format!("blink-test-both-{}", std::process::id());
        let _cleanup = test_support::CheckpointCleanup::new(&name);
        for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
            let mut cp = Checkpoint::new(&name, kind, vec![dl(0, JobStatus::Pending)]);
            cp.flush().expect("write the checkpoint");
        }

        let offers = offers_for(&name);

        assert_eq!(offers.len(), 2);
        assert!(offers.iter().any(|o| o.kind == CheckpointKind::Download));
        assert!(offers.iter().any(|o| o.kind == CheckpointKind::Upload));
    }

    #[test]
    fn an_unreadable_checkpoint_is_skipped_rather_than_propagated() {
        // Connecting must never fail because a checkpoint won't parse.
        let name = format!("blink-test-corrupt-{}", std::process::id());
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        assert!(offers_for(&name).is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet offer_tests`
Expected: FAIL to compile — `cannot find type CheckpointOffer`, `no method to_offer`, `cannot find function offers_for`.

- [x] **Step 3: Implement**

In `src/checkpoint.rs`, add `SystemTime` to the imports:

```rust
use std::time::{Duration, Instant, SystemTime};
```

Add the type and functions after the `Checkpoint` impl block:

```rust
/// A display-only summary of a checkpoint that still has work to do.
///
/// Everything here is for rendering. `sample_paths` is sanitized at
/// construction because the panel draws it directly rather than going
/// through `push_log`, which is where sanitization otherwise happens
/// centrally — and remote paths carry the server's own bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointOffer {
    pub kind: CheckpointKind,
    pub session: String,
    /// Jobs still to run: pending plus in-progress.
    pub remaining: usize,
    /// `remaining + done`. Cancelled jobs are excluded — that is work the
    /// user already abandoned, and counting it would overstate the total.
    pub total: usize,
    /// How long ago the checkpoint file was last written, if it could be
    /// stat'ed.
    pub age: Option<Duration>,
    /// Up to three outstanding paths, taken from the source side.
    pub sample_paths: Vec<String>,
}

/// The path a job reads *from* — what the user selected, and so what they
/// will recognise in the panel.
fn source_path(job: &CheckpointJob) -> String {
    match job {
        CheckpointJob::Download { remote_path, .. } => remote_path.clone(),
        CheckpointJob::Upload { local_path, .. } => local_path.display().to_string(),
        CheckpointJob::Mkdir { remote_path, .. } => remote_path.clone(),
    }
}

/// How long ago `path` was modified.
fn age_of(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

impl Checkpoint {
    /// Summarise this checkpoint for the resume panel.
    pub fn to_offer(&self, age: Option<Duration>) -> CheckpointOffer {
        let remaining = self.pending_count();
        let done = self.done_count();
        let sample_paths = self
            .jobs
            .iter()
            .filter(|j| j.needs_resume())
            .take(3)
            .map(|j| crate::error::sanitize(source_path(j)))
            .collect();
        CheckpointOffer {
            kind: self.kind,
            session: self.session.clone(),
            remaining,
            total: remaining + done,
            age,
            sample_paths,
        }
    }
}

/// Summaries of every checkpoint for `session` that still has work left.
///
/// A file that is absent, empty of outstanding work, or unreadable yields
/// no offer: connecting must never fail because of a checkpoint.
pub fn offers_for(session: &str) -> Vec<CheckpointOffer> {
    let mut out = Vec::new();
    for kind in [CheckpointKind::Download, CheckpointKind::Upload] {
        let Ok(path) = Checkpoint::path_for(session, kind) else {
            continue;
        };
        let cp = match Checkpoint::load_from(&path) {
            Ok(Some(cp)) => cp,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(?path, "skipping unreadable checkpoint: {e}");
                continue;
            }
        };
        if cp.pending_count() == 0 {
            continue;
        }
        out.push(cp.to_offer(age_of(&path)));
    }
    out
}
```

`path_for` is currently private. Change its signature to `pub(crate) fn path_for` so the test module and `offers_for` can both reach it.

- [x] **Step 4: Run tests**

Run: `cargo test --quiet offer_tests`
Expected: PASS (10 tests).

Then confirm the sanitization test is load-bearing — it is the one most likely to have compiled straight to green. Temporarily change `to_offer` to `.map(source_path)` (dropping the sanitize call) and re-run:

Run: `cargo test --quiet sample_paths_are_sanitized`
Expected: FAIL — `bidi override reached the panel`. Restore the sanitize call.

- [x] **Step 5: Full suite and commit**

Run: `cargo test --quiet && cargo clippy --all-targets -- -D warnings`

```bash
git add src/checkpoint.rs
git commit -m "feat(checkpoint): summarise checkpoints that still have work

CheckpointOffer is a display-only summary — direction, counts, age, and up
to three outstanding source paths. Paths are sanitized at construction
because the panel renders them directly rather than through push_log.

offers_for skips anything absent, finished, or unreadable: connecting must
never fail because of a checkpoint."
```

---

## Task 5: `discard`

**Files:**
- Modify: `src/checkpoint.rs`

**Interfaces:**
- Consumes: `DiscardOutcome` and `remove_orphan_parts` (Task 3).
- Produces: `pub fn discard(session: &str, kind: CheckpointKind) -> Result<DiscardOutcome>`

- [x] **Step 1: Write the failing test**

Add to `mod sweep_tests` in `src/checkpoint.rs`:

```rust
    #[test]
    fn discarding_removes_the_file_and_its_partials() {
        let dir = scratch("discard");
        let name = format!("blink-test-discard-{}", std::process::id());
        let _cleanup = super::test_support::CheckpointCleanup::new(&name);
        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let CheckpointJob::Download { local_path, .. } = &unfinished else { unreachable!() };
        std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();

        let mut cp = Checkpoint::new(&name, CheckpointKind::Download, vec![unfinished]);
        cp.flush().expect("write the checkpoint");
        let path = Checkpoint::path_for(&name, CheckpointKind::Download).unwrap();

        let outcome = discard(&name, CheckpointKind::Download).expect("discard");

        assert_eq!(outcome.parts_removed, 1);
        assert!(!path.exists(), "the checkpoint file must be gone");
        assert!(
            std::fs::metadata(crate::transport::part_path(&dir.join("a.bin"))).is_err(),
            "its orphaned partial must be gone too — nothing else records where it is",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discarding_a_checkpoint_that_is_already_gone_is_fine() {
        let name = format!("blink-test-absent-{}", std::process::id());
        let outcome = discard(&name, CheckpointKind::Download).expect("must not error");
        assert_eq!(outcome, DiscardOutcome::default());
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet discarding`
Expected: FAIL to compile — `cannot find function discard`.

- [x] **Step 3: Implement**

Add to `src/checkpoint.rs`, after `offers_for`:

```rust
/// Remove a checkpoint and the partial downloads it is the only record of.
///
/// The checkpoint names where every unfinished download left its `.part`
/// file; delete it without sweeping and those files are stranded with
/// nothing left to reference them. `remove_orphan_parts` skips `Done` jobs,
/// so only partials of transfers that never finished are removed.
///
/// Idempotent: a checkpoint that is already gone reports nothing removed.
pub fn discard(session: &str, kind: CheckpointKind) -> Result<DiscardOutcome> {
    let outcome = match Checkpoint::load(session, kind) {
        Ok(Some(cp)) => remove_orphan_parts(&cp),
        // Absent, or unreadable — either way there is nothing to sweep, but
        // the file (if any) should still go.
        Ok(None) => DiscardOutcome::default(),
        Err(e) => {
            tracing::warn!(session, "discarding an unreadable checkpoint: {e}");
            DiscardOutcome::default()
        }
    };
    Checkpoint::remove(session, kind)?;
    Ok(outcome)
}
```

- [x] **Step 4: Run tests**

Run: `cargo test --quiet && cargo clippy --all-targets -- -D warnings`
Expected: all pass, clippy silent.

- [x] **Step 5: Commit**

```bash
git add src/checkpoint.rs
git commit -m "feat(checkpoint): discard a checkpoint and its orphaned partials

The checkpoint is the only record of where an unfinished download left its
.part file, so removing it without sweeping strands those files. Matches
what blink checkpoints --clean already does."
```

---

## Task 6: The resume offer — queue, handler, and panel

`Screen` is matched exhaustively with no catch-all arm in `draw`, so the
moment `Screen::OfferResumeCheckpoint` exists, `draw` and `handle_key` must
both handle it or the crate does not compile. The variant, the handler that
answers it, and the view that renders it are therefore one atomic change —
this is the smallest unit that ends green.

It runs in three parts. Parts A and B do not build on their own; the test
gate and the commit are at the end of Part C.

### Part A — the queue

**Files:**
- Modify: `src/tui/state.rs` (`PostConnectOffer`)
- Modify: `src/tui/app/mod.rs` (`Screen`, `pending_offers`, `show_next_offer`, `draw` arm)
- Modify: `src/tui/app/events.rs` (`Connected`)

**Interfaces:**
- Consumes: `CheckpointOffer`, `offers_for` (Task 4).
- Produces:
  - `pub enum PostConnectOffer { ResumeCheckpoint(CheckpointOffer), SaveSession }`
  - `Screen::OfferResumeCheckpoint`
  - `App::pending_offers: VecDeque<PostConnectOffer>`
  - `App::show_next_offer(&mut self)`

- [x] **Step 1: Write the failing test**

Add to the `tests` module in `src/tui/app/mod.rs`:

```rust
    fn offer(kind: CheckpointKind) -> PostConnectOffer {
        PostConnectOffer::ResumeCheckpoint(crate::checkpoint::CheckpointOffer {
            kind,
            session: "s".into(),
            remaining: 1,
            total: 2,
            age: None,
            sample_paths: vec!["/srv/a.bin".into()],
        })
    }

    #[test]
    fn the_offer_queue_walks_to_each_screen_in_turn() {
        let mut a = app();
        a.pending_offers = std::collections::VecDeque::from(vec![
            offer(CheckpointKind::Download),
            PostConnectOffer::SaveSession,
        ]);

        a.show_next_offer();
        assert_eq!(a.screen, Screen::OfferResumeCheckpoint);

        a.pending_offers.pop_front();
        a.show_next_offer();
        assert_eq!(a.screen, Screen::OfferSaveSession);

        a.pending_offers.pop_front();
        a.show_next_offer();
        assert_eq!(a.screen, Screen::Main, "an empty queue lands on the main view");
    }

    #[test]
    fn an_empty_offer_queue_goes_straight_to_main() {
        let mut a = app();
        a.show_next_offer();
        assert_eq!(a.screen, Screen::Main);
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet offer_queue`
Expected: FAIL to compile — `cannot find type PostConnectOffer`, `no field pending_offers`, `no variant OfferResumeCheckpoint`.

- [x] **Step 3: Implement**

In `src/tui/state.rs`, add after the `PaneState` impl:

```rust
/// Something to ask the user immediately after a connection comes up.
///
/// Held in a queue so one place owns "what happens after connect" — the
/// order matters, and splitting it across the handlers that answer each
/// offer is how that ordering drifts.
#[derive(Debug, Clone)]
pub enum PostConnectOffer {
    /// A previous batch left work unfinished. Carries its summary.
    ResumeCheckpoint(crate::checkpoint::CheckpointOffer),
    /// The connection isn't backed by a saved session.
    SaveSession,
}
```

In `src/tui/app/mod.rs`:

Add the field to `App` next to `pending_session_unsaved`:

```rust
    /// Questions to ask once the connection is up, in order. Checkpoints
    /// first, then the save offer — see `show_next_offer`.
    pending_offers: std::collections::VecDeque<PostConnectOffer>,
```

Initialise it in `App::new`:

```rust
            pending_offers: std::collections::VecDeque::new(),
```

Import `PostConnectOffer` in the `crate::tui::state::{...}` use list.

Add the method to the `impl App` block, next to `push_log`:

```rust
    /// Show the next post-connect offer, or fall through to the main view.
    ///
    /// The offer stays at the front of the queue while it is displayed and
    /// is popped by whichever handler answers it.
    pub(super) fn show_next_offer(&mut self) {
        self.screen = match self.pending_offers.front() {
            Some(PostConnectOffer::ResumeCheckpoint(_)) => Screen::OfferResumeCheckpoint,
            Some(PostConnectOffer::SaveSession) => Screen::OfferSaveSession,
            None => Screen::Main,
        };
    }
```

Add `Screen::OfferResumeCheckpoint` in Part C, together with its `draw` and `handle_key` arms — `draw` matches `Screen` with no catch-all, so the variant and both arms have to arrive in the same edit.

In `src/tui/app/events.rs`, replace the `Connected` handler's screen decision:

```rust
                // Ask about unfinished work before asking about saving the
                // session: checkpoints are keyed by session *name*, and
                // accepting the save offer can rename it.
                let mut offers: std::collections::VecDeque<PostConnectOffer> =
                    crate::checkpoint::offers_for(&session.name)
                        .into_iter()
                        .map(PostConnectOffer::ResumeCheckpoint)
                        .collect();
                if std::mem::take(&mut self.pending_session_unsaved) {
                    offers.push_back(PostConnectOffer::SaveSession);
                }
                self.pending_offers = offers;
```

and replace the existing:

```rust
                let offer_save = std::mem::take(&mut self.pending_session_unsaved);
                self.screen = if offer_save {
                    Screen::OfferSaveSession
                } else {
                    Screen::Main
                };
```

with `self.show_next_offer();` — placed after `self.current_session = Some(session);` so `session.name` is read before the move. Import `PostConnectOffer` in `events.rs`.

Part A does not compile on its own — `show_next_offer` names
`Screen::OfferResumeCheckpoint`, which Part C introduces along with its match
arms. Continue straight to Part B.

### Part B — answering it

**Files:**
- Modify: `src/tui/app/handlers.rs` (`handle_offer_resume_checkpoint`, `handle_offer_save_session`)
- Modify: `src/tui/app/mod.rs` (`handle_key` arm)

**Interfaces:**
- Consumes: `PostConnectOffer`, `show_next_offer` (Task 6); `discard` (Task 5); existing `App::resume_walk`.
- Produces: `App::handle_offer_resume_checkpoint(&mut self, key: KeyEvent)`

- [x] **Step 1: Write the failing test**

Add to the `tests` module in `src/tui/app/mod.rs`:

```rust
    /// An app with one download checkpoint on disk and its offer queued,
    /// plus the cleanup guard the caller must hold for the test's lifetime.
    fn app_with_queued_offer(
        tag: &str,
    ) -> (App, crate::checkpoint::test_support::CheckpointCleanup) {
        let (mut a, cleanup) = checkpoint_app(tag);
        let name = a.current_session.as_ref().unwrap().name.clone();
        let mut cp = crate::checkpoint::Checkpoint::new(
            &name,
            CheckpointKind::Download,
            vec![crate::checkpoint::CheckpointJob::Download {
                remote_path: "/srv/a.bin".into(),
                local_path: std::path::PathBuf::from("/tmp/blink-test-a.bin"),
                status: crate::checkpoint::JobStatus::Pending,
            }],
        );
        cp.flush().expect("write the checkpoint");
        a.pending_offers = std::collections::VecDeque::from(vec![
            PostConnectOffer::ResumeCheckpoint(cp.to_offer(None)),
        ]);
        a.show_next_offer();
        (a, cleanup)
    }

    #[tokio::test]
    async fn resuming_from_the_offer_queues_the_outstanding_work() {
        let (mut a, _cleanup) = app_with_queued_offer("resume");

        a.handle_offer_resume_checkpoint(press(KeyCode::Char('r')));

        let queued = a.transfer_manager.as_ref().unwrap().queue_counts();
        assert!(queued.1 > 0 || queued.0 > 0, "the outstanding job must be queued");
        assert_eq!(a.screen, Screen::Main, "the queue is empty, so we land on Main");

    }

    #[tokio::test]
    async fn discarding_from_the_offer_removes_the_checkpoint() {
        let (mut a, _cleanup) = app_with_queued_offer("discard");
        let name = a.current_session.as_ref().unwrap().name.clone();

        a.handle_offer_resume_checkpoint(press(KeyCode::Char('d')));

        assert!(
            crate::checkpoint::offers_for(&name).is_empty(),
            "the checkpoint must be gone",
        );
        assert_eq!(a.screen, Screen::Main);

    }

    #[tokio::test]
    async fn deferring_the_offer_leaves_the_checkpoint_on_disk() {
        let (mut a, _cleanup) = app_with_queued_offer("later");
        let name = a.current_session.as_ref().unwrap().name.clone();

        a.handle_offer_resume_checkpoint(press(KeyCode::Esc));

        assert_eq!(
            crate::checkpoint::offers_for(&name).len(),
            1,
            "later means later, not never",
        );
        assert_eq!(a.screen, Screen::Main);

    }

    #[tokio::test]
    async fn the_resume_offer_ignores_keys_it_does_not_list() {
        let (mut a, _cleanup) = app_with_queued_offer("stray");

        for code in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Tab] {
            a.handle_offer_resume_checkpoint(press(code));
            assert_eq!(
                a.screen,
                Screen::OfferResumeCheckpoint,
                "{code:?} must not dismiss the offer",
            );
        }

    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet offer_ | grep -E "resuming_from|discarding_from|deferring"`
Expected: FAIL to compile — `no method handle_offer_resume_checkpoint`.

- [x] **Step 3: Implement**

Add to `impl App` in `src/tui/app/handlers.rs`:

```rust
    /// The offer to resume an interrupted batch, shown once per checkpoint
    /// after a connection comes up.
    ///
    /// `later` is deliberately non-destructive: the checkpoint stays on disk
    /// and is offered again next connect. `discard` is the way out, and it
    /// sweeps the batch's orphaned partials because the checkpoint is the
    /// only record of where they are.
    pub(super) fn handle_offer_resume_checkpoint(&mut self, key: KeyEvent) {
        let Some(PostConnectOffer::ResumeCheckpoint(offer)) =
            self.pending_offers.front().cloned()
        else {
            // Nothing queued — shouldn't happen, but don't strand the user.
            self.show_next_offer();
            return;
        };

        let direction = match offer.kind {
            CheckpointKind::Download => Direction::Download,
            CheckpointKind::Upload => Direction::Upload,
        };

        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.pending_offers.pop_front();
                self.resume_walk(direction);
                self.show_next_offer();
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.pending_offers.pop_front();
                match crate::checkpoint::discard(&offer.session, offer.kind) {
                    Ok(outcome) => {
                        let parts = outcome.parts_removed;
                        self.push_log(
                            LogLevel::Info,
                            if parts > 0 {
                                format!(
                                    "discarded the {} checkpoint and {parts} partial download(s)",
                                    offer.kind.as_str()
                                )
                            } else {
                                format!("discarded the {} checkpoint", offer.kind.as_str())
                            },
                        );
                        for failure in outcome.failures {
                            self.push_log(LogLevel::Warn, failure);
                        }
                    }
                    Err(e) => {
                        self.push_log(
                            LogLevel::Error,
                            format!("could not discard the checkpoint: {e}"),
                        );
                    }
                }
                self.show_next_offer();
            }
            KeyCode::Esc | KeyCode::Char('l') | KeyCode::Char('L') => {
                self.pending_offers.pop_front();
                self.push_log(
                    LogLevel::Info,
                    format!(
                        "{} checkpoint kept — press {} in the transfers pane to resume it",
                        offer.kind.as_str(),
                        match offer.kind {
                            CheckpointKind::Download => "r",
                            CheckpointKind::Upload => "R",
                        }
                    ),
                );
                self.show_next_offer();
            }
            _ => {}
        }
    }
```

Add the imports `handlers.rs` needs: `use crate::checkpoint::CheckpointKind;` and `use crate::tui::state::PostConnectOffer;` (add to the existing `crate::tui::state::{...}` list).

Update `handle_offer_save_session` so it participates in the queue. Replace its two arms:

```rust
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.pending_offers.pop_front();
                self.screen = Screen::Main;
                self.open_save_session();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.pending_offers.pop_front();
                self.show_next_offer();
                self.push_log(
                    LogLevel::Info,
                    "not saved — press ctrl+s any time to save this session".into(),
                );
            }
```

`SaveSession` is always last in the queue, so `y` handing off to the save modal loses nothing.

Add the `handle_key` arm in `src/tui/app/mod.rs`, next to `Screen::OfferSaveSession`:

```rust
            Screen::OfferResumeCheckpoint => self.handle_offer_resume_checkpoint(key),
```

Part B still does not compile — `Screen::OfferResumeCheckpoint` and its match
arms arrive in Part C. Continue.

### Part C — the panel, and the arms that make it reachable

**Files:**
- Modify: `src/tui/views.rs` (new `offer_resume_checkpoint` module)
- Modify: `src/tui/app/mod.rs` (`draw` arm)

**Interfaces:**
- Consumes: `CheckpointOffer` (Task 4), `App::pending_offers` (Task 6).
- Produces: `views::offer_resume_checkpoint::render(f: &mut Frame, app: &App)`

There is no terminal harness in this repo, so this task's gate is a compiling, clippy-clean build plus the manual check at the end of the plan. The one piece of pure logic — age formatting — is unit-tested.

- [x] **Step 1: Write the failing test**

Create the module in `src/tui/views.rs` after `pub mod offer_save_session`, containing **only** `human_age` and its tests for now — `render` arrives in Step 3:

```rust
pub mod offer_resume_checkpoint {
    use super::*;
    use crate::tui::state::PostConnectOffer;

    /// Coarse, human-readable age. Precision past the unit is noise here —
    /// the question the user is answering is "is this batch still relevant".
    fn human_age(d: std::time::Duration) -> String {
        let secs = d.as_secs();
        let (n, unit) = if secs < 60 {
            return "just now".to_string();
        } else if secs < 3600 {
            (secs / 60, "minute")
        } else if secs < 86_400 {
            (secs / 3600, "hour")
        } else {
            (secs / 86_400, "day")
        };
        let plural = if n == 1 { "" } else { "s" };
        format!("{n} {unit}{plural} ago")
    }
}
```

Then the tests, inside that module:

```rust
#[cfg(test)]
mod age_tests {
    use super::human_age;
    use std::time::Duration;

    #[test]
    fn recent_ages_read_as_minutes() {
        assert_eq!(human_age(Duration::from_secs(90)), "1 minute ago");
        assert_eq!(human_age(Duration::from_secs(600)), "10 minutes ago");
    }

    #[test]
    fn hours_and_days_round_down() {
        assert_eq!(human_age(Duration::from_secs(3 * 3600 + 59 * 60)), "3 hours ago");
        assert_eq!(human_age(Duration::from_secs(50 * 3600)), "2 days ago");
    }

    #[test]
    fn the_first_minute_reads_as_just_now() {
        assert_eq!(human_age(Duration::from_secs(5)), "just now");
    }

    #[test]
    fn singular_and_plural_are_both_handled() {
        assert_eq!(human_age(Duration::from_secs(3600)), "1 hour ago");
        assert_eq!(human_age(Duration::from_secs(24 * 3600)), "1 day ago");
    }
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test --quiet age_tests`
Expected: FAIL to compile — `cannot find function human_age`.

- [x] **Step 3: Implement**

Add `render` to the `offer_resume_checkpoint` module created in Step 1, above `mod age_tests`:

```rust
    pub fn render(f: &mut Frame, app: &App) {
        let Some(PostConnectOffer::ResumeCheckpoint(offer)) = app.pending_offers_front() else {
            return;
        };

        let area = f.area();
        let modal = super::centered_rect(60, 42, area);
        f.render_widget(Clear, modal);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.accent))
            .title(Span::styled(
                " resume interrupted transfer? ",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(modal);
        f.render_widget(block, modal);

        let dim = Style::default().fg(app.theme.dim);
        let fg = Style::default().fg(app.theme.fg);
        let width = inner.width.saturating_sub(4) as usize;

        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("an interrupted {} batch was found", offer.kind.as_str()),
                fg,
            ))
            .alignment(Alignment::Center),
            Line::from(""),
        ];

        let counts = format!("{} of {} items remaining", offer.remaining, offer.total);
        let headline = match &offer.age {
            Some(age) => format!("{counts} · {}", human_age(*age)),
            None => counts,
        };
        lines.push(
            Line::from(Span::styled(headline, fg.add_modifier(Modifier::BOLD)))
                .alignment(Alignment::Center),
        );
        lines.push(Line::from(""));

        // Already sanitized in `CheckpointOffer`; only width remains.
        for path in &offer.sample_paths {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(crate::tui::widgets::truncate_middle(path, width), dim),
            ]));
        }
        if offer.remaining > offer.sample_paths.len() {
            let more = offer.remaining - offer.sample_paths.len();
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("… and {more} more"), dim),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(
            Line::from(vec![
                Span::styled(
                    "[r]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" resume   ", dim),
                Span::styled(
                    "[d]",
                    Style::default()
                        .fg(app.theme.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" discard   ", dim),
                Span::styled(
                    "[esc]",
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" later", dim),
            ])
            .alignment(Alignment::Center),
        );

        f.render_widget(Paragraph::new(lines), inner);
    }

    // `mod age_tests` from Step 1 stays exactly as written, at the end of
    // this module.
}
```

Two things this needs that don't exist yet.

**Share `truncate_middle` rather than copying it.** It already exists in `src/tui/widgets.rs`, private inside `mod bottom_pane`, and does exactly this job for the transfers pane. Move it to `widgets` module scope and widen it:

```rust
// src/tui/widgets.rs — at module scope, outside `mod bottom_pane`
/// Truncate `s` to `max` chars by replacing the middle with `…`, so both
/// the head and the tail of a long path stay readable.
pub(crate) fn truncate_middle(s: &str, max: usize) -> String {
    // … body moved verbatim from `bottom_pane::truncate_middle` …
}
```

Delete the copy in `bottom_pane` and update its two call sites (`render_transfer_line`, and the `tests` module in that file) to `super::truncate_middle`. The view then calls `crate::tui::widgets::truncate_middle`. Its existing tests move with it.

**A queue accessor.** In `src/tui/app/mod.rs`, so the view can read the queue without the field being public:

```rust
    /// The offer currently being shown, if any. Used by its renderer.
    pub(crate) fn pending_offers_front(&self) -> Option<&PostConnectOffer> {
        self.pending_offers.front()
    }
```

Now add the `Screen` variant and both match arms in `src/tui/app/mod.rs` — this is the edit that makes the crate compile again.

Next to `OfferSaveSession` in the `Screen` enum:

```rust
    /// Modal over Main: a previous batch to this session left work
    /// unfinished — offer to resume it.
    OfferResumeCheckpoint,
```

In `draw`, next to the `Screen::OfferSaveSession` arm:

```rust
            Screen::OfferResumeCheckpoint => {
                crate::tui::views::main::render(f, self);
                crate::tui::views::offer_resume_checkpoint::render(f, self);
            }
```

In `handle_key`, next to the `Screen::OfferSaveSession` arm:

```rust
            Screen::OfferResumeCheckpoint => self.handle_offer_resume_checkpoint(key),
```

- [x] **Step 4: Run the whole task's tests**

This is the first point since Part A at which the crate compiles.

Run: `cargo test --quiet && cargo clippy --all-targets -- -D warnings && cargo build --release`
Expected: all pass, clippy silent, release builds.

If any of the queue or handler tests compiled straight to green, re-break the
implementation to confirm they catch it: make `show_next_offer` always return
`Screen::Main` and check the queue test fails; restore it.

- [x] **Step 5: Commit**

```bash
git add src/tui/state.rs src/tui/app/mod.rs src/tui/app/handlers.rs src/tui/app/events.rs src/tui/views.rs src/tui/widgets.rs
git commit -m "feat(tui): offer to resume an interrupted batch on connect

A VecDeque owns post-connect sequencing rather than each handler deciding
what follows it; checkpoints are offered before the save-session offer,
because they are keyed by session name and accepting the save can rename
the session.

r resumes through the existing resume_walk, d discards the checkpoint and
sweeps its orphaned partials, esc defers — non-destructive by design, so
the offer returns next connect and the log says how to reach it meanwhile.

The panel shows direction, how much is left, how old the checkpoint is,
and up to three outstanding paths — the paths are what make a batch
recognisable weeks later.

Screen is matched exhaustively with no catch-all, so the variant, its two
match arms, the handler, and the view had to land together."
```

---

## Task 7: Documentation

**Files:**
- Modify: `README.md`

**Interfaces:** none.

- [x] **Step 1: Document the panel**

In the walk-checkpointing bullet (around line 60), after the sentence ending "…re-queue only the jobs that didn't complete", add:

```markdown
  Connecting to a session that has an unfinished batch offers to resume it,
  with a summary of what is left. `[r]` resumes, `[d]` discards the
  checkpoint *and* the partial downloads it is the only record of, and
  `[esc]` defers — the offer returns on the next connect. Use `blink
  checkpoints` to inspect pending checkpoints from the command line.
```

Remove the now-duplicated "Use `blink checkpoints` to inspect…" sentence from the original bullet.

- [x] **Step 2: Note the name-keying consequence**

In the same section, add:

```markdown
  Checkpoints are keyed by session *name*, not by host. An ad-hoc
  `blink connect sftp://host` therefore matches a checkpoint belonging to a
  saved session called `host`, and editing a saved session's host leaves its
  old checkpoint in place. Check the paths in the summary if that is a
  possibility.
```

- [x] **Step 3: Fix the stale source-tree reference**

Line 499 reads:

```
        ├── checkpoint_glue.rs  dispatch_plan / resume_walk / discard_active_checkpoint
```

`discard_active_checkpoint` was renamed during the audit work. Replace with:

```
        ├── checkpoint_glue.rs  dispatch_plan / resume_walk / settle_checkpoint
```

- [x] **Step 4: Verify**

Run: `grep -n "discard_active_checkpoint" README.md`
Expected: no matches.

- [x] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: document the checkpoint resume offer

Also fixes a stale reference to discard_active_checkpoint, renamed during
the audit work, and records that checkpoints are keyed by session name
rather than by host."
```

---

## Manual Verification Checklist

No terminal harness exists in this repo, so the panel itself is checked by hand. Against any reachable SFTP server:

- [ ] Start a recursive download of a directory with several files, quit blink mid-batch (`q`, confirm), reconnect to the same session → panel appears, counts look right, paths are recognisable.
- [ ] Press `r` → the outstanding jobs queue and run; the panel does not reappear on the next connect once they finish.
- [ ] Repeat, press `esc` → connection proceeds to the main view; reconnect → panel appears again.
- [ ] Repeat, press `d` → panel goes away, log reports the checkpoint and any partials removed; `blink checkpoints` shows nothing for that session; the `.part` files are gone from the download directory.
- [ ] Interrupt both a download and an upload batch on the same session → two panels in turn, download first.
- [ ] Connect with `blink connect sftp://user@host` to a host with no saved session → save offer still appears, after any checkpoint offers.
- [ ] Interrupt a batch, disconnect with `ctrl+x` rather than quitting, reconnect in the same run → panel appears and `r` works (this is the Task 2 prerequisite).
- [ ] Corrupt a checkpoint file by hand (`echo '{' > …json`) → connecting still succeeds, no panel for that direction.
