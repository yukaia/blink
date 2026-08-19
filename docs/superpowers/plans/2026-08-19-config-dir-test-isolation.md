# Config Directory Test Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make it impossible for a test to read or write the user's real config directory, and use that to test the one `discard` property that is currently unreachable.

**Architecture:** `paths::base_dir()` is the single chokepoint every path function flows through. It gains a `#[cfg(test)]` branch returning a temporary directory — a private one per test when a `TestHome` guard is held, otherwise one shared per-process scratch directory. No production signature changes. The `CheckpointCleanup` drop guard, which existed only to compensate for the missing injection point, is then deleted.

**Tech Stack:** Rust 2024, `std::thread_local!`, `std::env::temp_dir()`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-19-checkpoint-test-isolation-design.md`

## Global Constraints

- No new dependencies, dev or otherwise. Temp directories follow the idiom already in the suite: `std::env::temp_dir()` joined with a `blink-`-prefixed tag and `std::process::id()`.
- No production signature changes. `paths`' public functions keep their names, arguments and return types.
- The thread-local override is sound only because each `#[test]` gets its own thread and every async test is `#[tokio::test]` with the default current-thread flavor. Do not add a `#[tokio::test(flavor = "multi_thread")]` that touches `paths`.
- The acceptance check for Tasks 1 and 2 is that `XDG_CONFIG_HOME=<empty dir> cargo test` leaves that directory completely empty.
- No existing test may regress. Expected totals, which the tasks assert: 366 at
  baseline, 371 after Task 1, 369 after Task 2 (it deletes the two `cleanup_tests`
  tests along with the guard they cover), 370 after Task 3.

---

## File Structure

| File | Responsibility |
|---|---|
| `src/paths.rs` | Owns the resolution of every application path. Gains the test-only redirect and the `TestHome` guard. This is the only file that knows the real directory exists. |
| `src/checkpoint.rs` | Loses `test_support::CheckpointCleanup` and `cleanup_tests`; its tests take a `TestHome` instead. Gains the new `discard` test. |
| `src/tui/app/mod.rs` | Two test helpers change the guard type they hand back. |

---

## Task 1: The isolation hook in `paths.rs`

**Files:**
- Modify: `src/paths.rs` — the four platform `base_dir` functions (lines 51–113), plus a new `test_home` module and a new `tests` module

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn paths::test_home() -> paths::TestHome` — acquires a private config home for the calling thread; the directory is removed when the guard drops.
  - `pub struct paths::TestHome` with `pub fn path(&self) -> &Path`.
  - Both are `#[cfg(test)]` only.

`src/paths.rs` has no test module today; Step 1 creates one.

- [ ] **Step 1: Write the failing tests**

Append to `src/paths.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_redirects_away_from_the_shared_home() {
        let shared = base_dir().expect("shared home");
        assert_eq!(
            shared,
            std::env::temp_dir().join(format!("blink-test-{}", std::process::id())),
            "without a guard, tests share one per-process scratch home",
        );

        let _home = test_home();
        assert_ne!(base_dir().expect("private home"), shared);
    }

    #[test]
    fn the_override_is_cleared_when_the_guard_drops() {
        let shared = base_dir().expect("shared home");
        {
            let _home = test_home();
        }
        assert_eq!(
            base_dir().expect("shared home again"),
            shared,
            "a dropped guard must not leave the thread pointing at a deleted directory",
        );
    }

    #[test]
    fn guards_on_different_threads_get_different_homes() {
        let _home = test_home();
        let mine = base_dir().expect("my home");
        let theirs = std::thread::spawn(|| {
            let _home = test_home();
            base_dir().expect("their home")
        })
        .join()
        .expect("the spawned thread must not panic");
        assert_ne!(mine, theirs, "isolation is per-test, not per-process");
    }

    #[test]
    fn a_guard_removes_its_tree_even_when_a_test_panics() {
        // Drop-on-unwind is the whole reason this is a guard rather than a
        // cleanup call at the end of a test: a test that fails is exactly
        // when its directory would otherwise be left behind.
        let payload = std::panic::catch_unwind(|| {
            let home = test_home();
            let sessions = sessions_dir().expect("sessions dir");
            std::fs::write(sessions.join("t.ini"), b"x").expect("write a session file");
            std::panic::panic_any(home.path().to_path_buf());
        })
        .expect_err("the closure must have panicked");

        let dir = *payload
            .downcast::<PathBuf>()
            .expect("the panic payload carries the guard's directory");
        assert!(!dir.exists(), "unwinding must still run the guard's Drop");
    }

    #[test]
    fn a_file_written_under_a_guard_is_gone_when_the_guard_drops() {
        let dir;
        {
            let home = test_home();
            dir = home.path().to_path_buf();
            let sessions = sessions_dir().expect("sessions dir");
            std::fs::write(sessions.join("t.ini"), b"x").expect("write a session file");
            assert!(sessions.join("t.ini").exists());
        }
        assert!(!dir.exists(), "the guard must remove its whole tree");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib paths::tests 2>&1 | tail -20`

Expected: compile failure — `cannot find function 'test_home' in this scope`.

- [ ] **Step 3: Rename the platform implementations**

In `src/paths.rs`, rename all four `base_dir` functions to `real_base_dir`, leaving their bodies and `#[cfg]` attributes untouched, and mark each unused-under-test. The four are the `target_os = "linux"`, `target_os = "macos"`, `target_os = "windows"` and the `not(any(...))` fallback.

Each declaration line changes from:

```rust
#[cfg(target_os = "linux")]
fn base_dir() -> Result<PathBuf> {
```

to:

```rust
#[cfg(target_os = "linux")]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
```

Apply the same two-line change to the other three. Do not touch anything inside the bodies.

- [ ] **Step 4: Add the dispatcher and the guard**

Insert immediately above the first `real_base_dir` in `src/paths.rs`:

```rust
/// Root of the application's data directory.
///
/// Under test this never resolves to the user's real directory — see
/// [`test_home`].
fn base_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(test_home::current())
    }
    #[cfg(not(test))]
    {
        real_base_dir()
    }
}

/// A private config home for one test, active until the guard drops.
///
/// Tests that write through `paths` need each other's writes kept apart:
/// without this they share one directory and race, which is why the
/// `discard` removal-failure property could not be tested before.
#[cfg(test)]
pub fn test_home() -> TestHome {
    test_home::acquire()
}

#[cfg(test)]
pub use test_home::TestHome;

#[cfg(test)]
mod test_home {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The config home for the current test.
    ///
    /// Thread-local because the test harness runs each `#[test]` on its own
    /// thread, and every async test in this crate is `#[tokio::test]` with
    /// the default current-thread flavor, so tasks it spawns stay on that
    /// thread. A `#[tokio::test(flavor = "multi_thread")]` that touched
    /// `paths` would see the shared home below instead of its own — read
    /// this comment before adding one.
    thread_local! {
        static OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// The home in force on this thread: a guard's private directory if one
    /// is held, otherwise the shared per-process scratch directory.
    ///
    /// The shared fallback is what makes the real directory unreachable
    /// rather than merely avoidable: a test written later that calls
    /// `Session::save()` without thinking about isolation lands here, not in
    /// the user's config.
    pub fn current() -> PathBuf {
        OVERRIDE
            .with(|o| o.borrow().clone())
            .unwrap_or_else(shared)
    }

    fn shared() -> PathBuf {
        std::env::temp_dir().join(format!("blink-test-{}", std::process::id()))
    }

    pub struct TestHome {
        dir: PathBuf,
    }

    impl TestHome {
        pub fn path(&self) -> &Path {
            &self.dir
        }
    }

    pub fn acquire() -> TestHome {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("blink-test-{}-{n}", std::process::id()));
        // A previous run with the same pid could have left this behind.
        let _ = std::fs::remove_dir_all(&dir);
        OVERRIDE.with(|o| *o.borrow_mut() = Some(dir.clone()));
        TestHome { dir }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            // Clear the override *before* removing the tree: if the removal
            // fails, a later call on this thread must not keep resolving to
            // a directory we just tried to delete.
            OVERRIDE.with(|o| *o.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}
```

- [ ] **Step 5: Run the new tests**

Run: `cargo test --lib paths::tests 2>&1 | tail -20`

Expected: PASS, 5 tests.

- [ ] **Step 6: Run the whole suite**

Run: `cargo test 2>&1 | tail -5`

Expected: 371 passed (366 existing + 5 new), 0 failed.

- [ ] **Step 7: Run the acceptance check**

```bash
D=$(mktemp -d)
XDG_CONFIG_HOME="$D" cargo test 2>&1 | tail -3
find "$D"
```

Expected: the tests pass, and `find` prints **only** `$D` itself. Before this task it printed `$D/blink`, `$D/blink/checkpoints`, `$D/blink/sessions` and `$D/blink/themes`.

- [ ] **Step 8: Commit**

```bash
git add src/paths.rs
git commit -m "test(paths): make the real config directory unreachable from tests

base_dir is the single chokepoint every path function flows through, so one
cfg(test) branch there isolates config, sessions, themes and checkpoints
together. A TestHome guard gives one test a private directory; tests that do
not ask for one share a per-process scratch directory, so the real config
directory cannot be reached even by a test that never considered it."
```

---

## Task 2: Retire `CheckpointCleanup`

**Files:**
- Modify: `src/checkpoint.rs` — delete `test_support` (lines 991–1017) and `cleanup_tests` (lines 1019–1053); convert 5 call sites; rewrite one comment. All line numbers here are as of the start of this task; Step 2's deletion shifts everything below it, which is why Step 3 locates its target by name.
- Modify: `src/tui/app/mod.rs:1372` and `:1445` — the guard type returned by two helpers

**Interfaces:**
- Consumes: `paths::test_home()` and `paths::TestHome` from Task 1.
- Produces: nothing new. `crate::checkpoint::test_support` no longer exists.

**No new test in this task.** It replaces test scaffolding with stronger scaffolding; the 370 tests from Task 1 are the test, and they must all still pass. Do not invent a test to satisfy TDD here.

- [ ] **Step 1: Convert the five `checkpoint.rs` call sites**

At each of lines 1135, 1349, 1368, 1385 and 1635, replace the cleanup guard with a home guard. The three forms currently in the file are:

```rust
let _cleanup = super::test_support::CheckpointCleanup::new(&name);
let _cleanup = test_support::CheckpointCleanup::new(&name);
```

Each becomes:

```rust
let _home = paths::test_home();
```

`paths` is already in scope in `checkpoint.rs`. Leave the surrounding `let name = format!("blink-test-...-{}", std::process::id());` lines alone — the pid in the name is now redundant but harmless, and removing it is churn this task does not need.

- [ ] **Step 2: Delete the guard and its tests**

Delete the whole `#[cfg(test)] pub(crate) mod test_support { ... }` block (lines 991–1017) and the whole `#[cfg(test)] mod cleanup_tests { ... }` block that follows it (lines 1019–1053). The two `CheckpointCleanup::new` uses inside `cleanup_tests` go with it — they are the guard testing itself.

- [ ] **Step 3: Rewrite the stale comment**

Step 2 shifted every line number below 991, so find this by name: inside the test `a_removal_failure_is_recorded_rather_than_propagated`, a comment now describes a type that no longer exists:

```rust
        // Clean up the substituted directory immediately — `Checkpoint::remove`
        // (and so `CheckpointCleanup`'s `Drop`) calls `remove_file`, which
        // cannot remove a directory, so nothing else will.
```

Replace with:

```rust
        // Clean up the substituted directory immediately: `Checkpoint::remove`
        // calls `remove_file`, which cannot remove a directory, so the test
        // home's own cleanup would leave it behind.
```

- [ ] **Step 4: Convert the two `tui/app/mod.rs` helpers**

At line 1372, change the signature and acquire the guard **first**, so everything the helper does afterwards resolves to the private directory:

```rust
    /// An app whose checkpoints go under a name no real session will use,
    /// plus the test home that keeps them out of every other test's way.
    fn checkpoint_app(tag: &str) -> (App, crate::paths::TestHome) {
        let home = crate::paths::test_home();
        let mut a = app();
        let name = format!("blink-test-{tag}-{}", std::process::id());
        let mut s = Session::from_url("sftp://me@host").unwrap();
        s.name = name.clone();
        a.current_session = Some(s);
        a.transfer_manager = Some(TransferManager::new(1).0);
        (a, home)
    }
```

At line 1445, change only the return type of `app_with_queued_offer`:

```rust
    fn app_with_queued_offer(
        tag: &str,
    ) -> (App, crate::paths::TestHome) {
```

Its body already binds the guard as `cleanup` from `checkpoint_app` and returns it as `(a, cleanup)`; leave that alone. The 10 `let (mut a, _cleanup) = ...` call sites need no change — only the type flowing through them differs.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test 2>&1 | tail -5`

Expected: **369** passed, 0 failed. The count *falls* by two: `cleanup_tests`
held two tests of the guard being deleted, and `TestHome`'s equivalent properties
are already covered by Task 1's `a_file_written_under_a_guard_is_gone_when_the_guard_drops`
and `a_guard_removes_its_tree_even_when_a_test_panics`. Do not add tests to reach a
higher number. A failure here means a test depended on the cleanup guard's exact semantics (removing a known session's checkpoints afterwards) rather than on isolation; read the failing test before changing anything else.

- [ ] **Step 6: Confirm the guard is really gone**

Run: `grep -rn "CheckpointCleanup\|test_support" src`

Expected: no matches.

- [ ] **Step 7: Commit**

```bash
git add src/checkpoint.rs src/tui/app/mod.rs
git commit -m "test(checkpoint): drop CheckpointCleanup for a private test home

The guard existed only to clean up after the missing injection point in
paths, which its own doc comment said. A private directory per test is
strictly stronger: the old guard removed a session's checkpoints afterwards,
whereas the directory is now the test's alone for its whole life."
```

---

## Task 3: The property that could not be tested

**Files:**
- Modify: `src/checkpoint.rs` — add one test to `sweep_tests`, trim the comment in `a_removal_failure_is_recorded_rather_than_propagated`

**Interfaces:**
- Consumes: `paths::test_home()` from Task 1; the existing `sweep_tests` helpers `scratch(tag) -> PathBuf` and `job(dir, name, status) -> CheckpointJob`.
- Produces: nothing.

`discard` sweeps orphaned `.part` files, then removes the checkpoint file. If that removal fails it records the failure in `outcome.failures` instead of returning `Err`, precisely so the sweep count survives. The existing test covers a checkpoint that is *unreadable and* undeletable, where the count is 0 anyway. The half that has never been tested is a **nonzero** count surviving a failed removal, which needs a checkpoint that reads fine but cannot be unlinked — a non-writable parent directory, which was unsafe to do while every test shared that directory.

- [ ] **Step 1: Write the failing test**

Add to the `sweep_tests` module in `src/checkpoint.rs`, after `a_removal_failure_is_recorded_rather_than_propagated`:

```rust
    /// The other half of `a_removal_failure_is_recorded_rather_than_propagated`:
    /// a checkpoint that reads fine, so the sweep runs and finds partials,
    /// but cannot be unlinked. Before the fix the `?` on `Checkpoint::remove`
    /// threw the whole `DiscardOutcome` away, losing a count of files that
    /// had *already* been deleted — the caller could not report what it had
    /// cleaned up. Only constructible with a test-private checkpoint
    /// directory: it works by making that directory read-only.
    #[cfg(unix)]
    #[test]
    fn a_removal_failure_keeps_the_count_the_sweep_already_earned() {
        use std::os::unix::fs::PermissionsExt;

        let _home = paths::test_home();
        let dir = scratch("remove-fail-sweep");
        let name = "remove-fail-sweep";

        let unfinished = job(&dir, "a.bin", JobStatus::Pending);
        let CheckpointJob::Download { local_path, .. } = &unfinished else {
            unreachable!()
        };
        std::fs::write(crate::transport::part_path(local_path), b"x").unwrap();

        let mut cp = Checkpoint::new(name, CheckpointKind::Download, vec![unfinished]);
        cp.flush().expect("write the checkpoint");

        let cp_dir = paths::checkpoints_dir().expect("checkpoints dir");
        let original = std::fs::metadata(&cp_dir).unwrap().permissions();
        std::fs::set_permissions(&cp_dir, std::fs::Permissions::from_mode(0o500))
            .expect("make the checkpoint directory read-only");

        // Root ignores the permission bits, so confirm the setup actually
        // bites before asserting on it.
        let probe = cp_dir.join("probe");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&cp_dir, original).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("skipped: this runner writes through a 0500 directory (root?)");
            return;
        }

        let outcome = discard(name, CheckpointKind::Download)
            .expect("discard must return Ok, not propagate the removal failure");

        // Restore before asserting: a failing assertion panics, and the test
        // home cannot remove a tree it has no write permission on.
        std::fs::set_permissions(&cp_dir, original).unwrap();

        assert_eq!(
            outcome.parts_removed, 1,
            "the partial was already deleted — that count must survive the failed unlink",
        );
        assert_eq!(
            outcome.failures.len(), 1,
            "and the failed unlink must still be reported",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run it to verify it passes, then prove it is load-bearing**

Run: `cargo test --lib a_removal_failure_keeps 2>&1 | tail -10`

Expected: PASS. This is a characterization test of behaviour that is already correct, so it cannot fail first. Prove it guards something by temporarily breaking the production code: in `discard` (`src/checkpoint.rs:691`), replace

```rust
    if let Err(e) = Checkpoint::remove(session, kind) {
        outcome
            .failures
            .push(format!("could not remove the checkpoint file: {e}"));
    }
```

with the pre-fix form:

```rust
    Checkpoint::remove(session, kind)?;
```

Run the test again.

Expected: FAIL on `discard must return Ok, not propagate the removal failure`. **Restore the original code before continuing** — `git diff src/checkpoint.rs` must show only the new test.

- [ ] **Step 3: Trim the superseded comment**

The 24-line comment in `a_removal_failure_is_recorded_rather_than_propagated` (from "Getting only the final unlink to fail…" down to "…instead.") argues that this case is not constructible. That is no longer true. Replace those paragraphs with:

```rust
        // This exercises "unreadable *and* undeletable": replacing the
        // checkpoint file with a directory makes reading it fail with the
        // same `IsADirectory` error as removing it, so `parts_removed` is 0
        // here because the load took the unreadable branch, not because the
        // fix dropped a count. The case where the load succeeds and only the
        // unlink fails is
        // `a_removal_failure_keeps_the_count_the_sweep_already_earned`.
```

Keep the first paragraph ("The property this guards: …") as it is.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test 2>&1 | tail -5`

Expected: 370 passed, 0 failed.

- [ ] **Step 5: Commit**

```bash
git add src/checkpoint.rs
git commit -m "test(checkpoint): cover a nonzero sweep count surviving a failed unlink

The existing test could only reach 'unreadable and undeletable', where the
count is 0 anyway. Getting a checkpoint that reads fine but cannot be
unlinked needs a read-only parent directory, which was unsafe while every
test shared that directory. With a private one it is three lines."
```

---

## Manual Verification

None. Every claim in this plan is checked by `cargo test` or by the
`XDG_CONFIG_HOME` acceptance check in Task 1 Step 7.
