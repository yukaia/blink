# Test Isolation for the Config Directory

**Date:** 2026-08-19  
**Status:** Approved  

## Summary

Under test, every `paths::` consumer resolves to the user's real config
directory. `paths::base_dir()` reads `$XDG_CONFIG_HOME` / `$HOME` and has no
injection point, so a test that saves a session, writes a config, or flushes a
checkpoint writes into `~/.config/blink/`. Checkpoints are the visible case —
`docs/BACKLOG.md` records it — but the cause is one function, and the fix
belongs there rather than in `checkpoint.rs`.

`base_dir()` gains a `#[cfg(test)]` branch that returns a temporary directory
instead, plus a guard giving an individual test a private one. Production
signatures do not change.

## What is actually broken

Measured, not assumed. Running the suite against an empty `XDG_CONFIG_HOME`:

```
$ XDG_CONFIG_HOME=<empty> cargo test      # 366 passed
$ find <empty>
<empty>/blink
<empty>/blink/checkpoints
<empty>/blink/sessions
<empty>/blink/themes
```

Three directories, no files. So the problem is narrower than "tests leave
checkpoints behind":

- **Residual files: none.** The `test_support::CheckpointCleanup` drop guard
  works, and `config.rs`, `session.rs`, `theme.rs` and `known_hosts.rs` avoid
  the issue by testing through their path-taking `load_from(&Path)` variants.
- **Transient writes: yes.** During a run, real checkpoint files exist in the
  user's directory between creation and cleanup. This is what blocks the
  untestable property below, and it means a `blink` process running during a
  test run can read a test's checkpoint.
- **Two historical strays** sit in the real directory —
  `session-download.json` (2026-05-21, format v2, pre-hash filename) and
  `session-3f3af1ec-download.json` (2026-08-09, format v3). Both are the
  fixture session `session` with `/remote/file0`. Neither run of the current
  suite produced them; they predate the drop guard. Since 0.6.0 offers a
  resume panel on connect, they are user-visible for anyone with a session
  named `session`.

The strays are **not** in scope. They belong to no saved session, so
`blink checkpoints --clean` already classifies them as orphaned and removes
them. That is a one-command user action, not a code change.

### The property that cannot be tested today

`discard` sweeps orphaned `.part` files, then removes the checkpoint file. If
the removal fails it records the failure in `outcome.failures` rather than
returning `Err`, specifically so the `parts_removed` count from the sweep is
not thrown away:

```rust
if let Err(e) = Checkpoint::remove(session, kind) {
    outcome.failures.push(format!("could not remove the checkpoint file: {e}"));
}
```

Forcing that removal to fail means making the containing directory
non-writable. Today that directory is shared with every other test in the run
and with the user's own blink, so the test would race everything. The property
is currently verified by reading the code, not by running it.

## Scope

- **In scope:** a `#[cfg(test)]` branch in `paths::base_dir`; a per-test guard;
  deleting `test_support::CheckpointCleanup` and its tests; converting its
  remaining call sites; the previously untestable `discard` test.
- **Out of scope:** the `CheckpointStore` refactor from `docs/BACKLOG.md` —
  once isolation exists it must justify itself as a design improvement rather
  than as test hygiene, which is a separate decision; the two stray files;
  changing any production signature; `default_local_dir`, which does not go
  through `base_dir`.

## Architecture

The four platform `base_dir` implementations are renamed `real_base_dir` and
left otherwise untouched. A single dispatcher takes the name:

```rust
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
```

Under test the real implementations are unreachable, so each carries
`#[cfg_attr(test, allow(dead_code))]`. They keep no test coverage today and
gain none here; that is a pre-existing gap, and env-var manipulation to close
it is `unsafe` in Rust 2024 and racy across threads.

`test_home::current()` resolves in two steps:

1. A thread-local override, set while a `TestHome` guard is alive. This is the
   per-test private directory.
2. Otherwise a single per-process scratch directory,
   `std::env::temp_dir()/blink-test-<pid>`, shared by every test that did not
   ask for isolation.

Step 2 is what makes the real directory unreachable rather than merely
avoidable. A test written next year that calls `Session::save()` without
thinking about isolation gets the shared scratch directory — the same
collision risk tests have among themselves today, but never the user's data.

```rust
#[cfg(test)]
mod test_home {
    /// Config home for the current test.
    ///
    /// Thread-local because the test harness runs each `#[test]` on its own
    /// thread, and every async test in this crate is `#[tokio::test]` with the
    /// default current-thread flavor, so spawned tasks stay on that thread.
    /// A `#[tokio::test(flavor = "multi_thread")]` touching `paths` would see
    /// the shared scratch directory instead of its own — check this comment
    /// before adding one.
    thread_local! {
        static OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub fn current() -> PathBuf;

    /// Private config home for one test. Removed when the guard drops.
    pub struct TestHome { dir: PathBuf }
}

// paths.rs
#[cfg(test)]
pub fn test_home() -> test_home::TestHome;
```

Directory naming follows the idiom already in the suite (`session.rs:1079`,
`checkpoint.rs:1062`): `temp_dir()` joined with a `blink-`-prefixed tag and the
pid, no new dependency. Per-test directories add an `AtomicUsize` counter so
concurrent tests cannot collide: `blink-test-<pid>-<n>`.

`TestHome::drop` clears the thread-local and removes the tree, in that order,
so a failure to remove cannot leave a stale override pointing at a deleted
path. The per-process scratch directory is never removed — nothing owns its
lifetime — and is left to the operating system's temp cleanup, matching what
the existing tests already do.

## Consequences for `checkpoint.rs`

`test_support::CheckpointCleanup` exists only to compensate for the missing
injection point. Its doc comment says so. With isolation, it and its
`cleanup_tests` module are deleted.

Of its 8 constructions, 2 are inside `cleanup_tests` — the guard testing
itself — and go with it. The remaining 6 become `let _home =
paths::test_home();`:

- 5 direct uses in `checkpoint.rs` (lines 1135, 1349, 1368, 1385, 1635)
- 1 in the `checkpoint_app` helper in `tui/app/mod.rs`

The `tui/app/mod.rs` conversion is mechanical rather than invasive. Both
helpers there — `checkpoint_app` and `app_with_queued_offer` — already return
`(App, CheckpointCleanup)` for the caller to hold, so only the type in the
return position changes; the 8 destructuring call sites keep their shape.

The comment at `checkpoint.rs:1197`, which explains a test in terms of
`CheckpointCleanup`'s `Drop` calling `remove_file`, has to be rewritten for
whatever that test does once the guard is gone.

This is strictly stronger than the guard: it removed a known session's
checkpoints afterwards, whereas the directory is now private for the test's
whole life.

The new test, previously impossible:

```rust
#[cfg(unix)]
#[test]
fn discard_keeps_its_swept_count_when_the_file_cannot_be_removed() {
    let _home = paths::test_home();
    // write a checkpoint with a pending download and a real .part file,
    // chmod the checkpoint directory to 0500, discard,
    // assert parts_removed == 1 and failures is non-empty,
    // chmod back so the guard can clean up.
}
```

`#[cfg(unix)]` because directory permissions are the mechanism; the property
holds on Windows but cannot be provoked the same way. The `chmod` back is not
optional — `TestHome::drop` cannot remove a tree it has no write permission
on.

## Testing

The acceptance check is the measurement from the top, re-run:

```
XDG_CONFIG_HOME=<empty dir> cargo test
find <empty dir>          # must print the directory itself and nothing else
```

This is end-to-end over the whole suite and stronger than any assertion inside
it: it proves no test reaches the real paths, including tests nobody thought
about.

Unit tests on the hook itself:

- a guard's directory differs from the shared scratch directory
- two guards on different threads get different directories
- a file written through `paths::sessions_dir()` under a guard is gone after
  the guard drops
- the override is cleared after the guard drops, so a later call in the same
  thread returns the shared directory

Plus the `discard` test above, and the existing 366 tests, which must keep
passing unchanged.

## Files Changed

| File | Change |
|---|---|
| `src/paths.rs` | Rename 4 platform `base_dir` → `real_base_dir`; add the dispatcher, the `test_home` module, and `test_home()`; add the hook's unit tests |
| `src/checkpoint.rs` | Delete `test_support` and `cleanup_tests`; convert 5 guard call sites; rewrite the comment at 1197; add the `discard` permission test |
| `src/tui/app/mod.rs` | Change the guard type returned by `checkpoint_app` and `app_with_queued_offer` |
