# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## Checkpoint test isolation

**Noted:** 2026-08-09, after 0.6.0.

**Problem.** Every checkpoint operation that resolves its own path goes through
`paths::checkpoints_dir()`, which has no injection point — `flush`,
`flush_if_due`, `load`, `remove`, `offers_for`, `discard`, `list_and_clean`.
Tests therefore write into the user's real `~/.config/blink/checkpoints/` and
can only clean up afterwards, via the `test_support::CheckpointCleanup` drop
guard.

That guard fixed *cleanup*. It did not fix *isolation*, and two things follow:

- Test runs have left stray checkpoints in the real directory (`session-*`,
  from an older test that flushed under a shared name). Since 0.6.0 surfaces
  checkpoints as a panel on connect, such strays are now user-visible.
- One property cannot be tested at all: that `discard` preserves its swept
  `parts_removed` count when `Checkpoint::remove` fails. Forcing the removal to
  fail needs the containing directory made non-writable, which would race every
  other test flushing to that same shared directory. The property is currently
  verified by reading the code (`outcome` is mutated in place, never replaced),
  not by a test.

**Shape.** A `CheckpointStore` owning the directory, with `Checkpoint` reduced
to plain data:

```rust
pub struct CheckpointStore { dir: PathBuf }

impl CheckpointStore {
    pub fn user() -> Result<Self>              // resolves paths::checkpoints_dir()
    pub fn at(dir: impl Into<PathBuf>) -> Self // tests
    pub fn load(&self, session, kind) -> Result<Option<Checkpoint>>
    pub fn flush(&self, cp: &mut Checkpoint) -> Result<()>
    pub fn offers_for(&self, session) -> Vec<CheckpointOffer>
    pub fn discard(&self, session, kind) -> Result<DiscardOutcome>
}
```

`App` gains a `checkpoint_store` field built in `App::new`. Roughly 250 lines
across `checkpoint.rs`, `events.rs`, `handlers.rs`, `checkpoint_glue.rs`,
`mod.rs` and `main.rs` — eight production call sites, plus the tests, which
stop needing the cleanup guard entirely.

A cheaper variant exists (add `_in` / `_at` path-taking variants beside the
current functions, ~60 lines, production untouched) but it leaves two parallel
APIs and still lets App-level tests write to the real directory. It unblocks
the one test without curing the pollution.

**Decide before starting.**

1. Does `flush` stay a method? `cp.flush()` reads better than
   `store.flush(&mut cp)`, but keeping it means `Checkpoint` carries a
   `#[serde(skip)] dir` and the directory lives in two places that can
   disagree.
2. Does this generalise? `sessions_dir`, `themes_dir` and `config_file` have
   the same shape, and `Session::save()` writes to the real directory under
   test today. An injected `Paths` value would fix all of them, at perhaps
   three times the work.
3. Is it worth it now? Nothing is broken. This buys test hygiene and one
   currently-unwritable test.

---

## `parallel_downloads` clamping is inconsistent between config and session

**Noted:** 2026-08-09, found by the 0.6.0 documentation audit.

`Config::load_from` warns only when the value is `0`, so a global
`parallel_downloads = 50` is silently reduced to the maximum of 10 with nothing
said. `Session::load_from` warns on any clamp. The README currently documents
the asymmetry as it stands.

Two lines to make the global path warn on any clamp, matching the session path.
