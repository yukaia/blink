# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## Should checkpoints go through a `CheckpointStore`?

**Noted:** 2026-08-09, after 0.6.0. **Reframed:** 2026-08-19, once test isolation
landed and stopped being the reason to do this.

The original entry argued for a `CheckpointStore` on the strength of test
hygiene: `paths::checkpoints_dir()` had no injection point, so checkpoint tests
wrote into the user's real directory and could only clean up afterwards.

That argument is spent. `paths::base_dir()` now redirects under `#[cfg(test)]`,
so no test can reach the real config directory at all, given the crate's
current shape — not for checkpoints, and not for sessions, themes or config
either. (The redirect depends on `cfg(test)` being set for every test that
runs, which holds only because there is no `[lib]` target and every test is
inline; a `[lib]` target plus a `tests/` directory would link this crate as
an ordinary dependency for integration tests, compiling it without
`cfg(test)` and reaching the real directory after all.) The `CheckpointCleanup`
drop guard is gone, and the one property that could not be tested before (a
nonzero `parts_removed` surviving a failed unlink) has a test.

What remains is a design question with no test-hygiene thumb on the scale:

```rust
pub struct CheckpointStore { dir: PathBuf }
```

with `Checkpoint` reduced to plain data, `App` holding a `checkpoint_store`, and
the eight call sites in `events.rs`, `handlers.rs`, `checkpoint_glue.rs` and
`main.rs` going through it. Roughly 250 lines.

The case for it is now purely that a type owning its directory is clearer than
free functions each re-deriving the path, and that `Checkpoint` currently mixes
data with its own persistence. The case against is that nothing is broken, the
free functions are short, and 250 lines of churn buys no behaviour.

Worth settling one way or the other rather than leaving it here indefinitely.
If it goes ahead, `flush` staying a method is the first thing to decide:
`cp.flush()` reads better than `store.flush(&mut cp)`, but keeping it means
`Checkpoint` carries a `#[serde(skip)] dir` and the directory lives in two
places that can disagree.

- Unrelated to the store question, but noticed while touching this area: the
  four platform `real_base_dir` implementations now carry
  `#[cfg_attr(test, allow(dead_code))]`, because `base_dir()` never calls
  them under `cfg(test)`. Their XDG/HOME absolute-path validation therefore
  has no test coverage. Extracting a pure
  `base_dir_from(xdg: Option<&str>, home: Option<&str>)` — taking the two
  env values as arguments instead of reading them — would make that logic
  directly testable without `unsafe` env-var manipulation.

---

## `parallel_downloads` clamping is inconsistent between config and session

**Noted:** 2026-08-09, found by the 0.6.0 documentation audit.

`Config::load_from` warns only when the value is `0`, so a global
`parallel_downloads = 50` is silently reduced to the maximum of 10 with nothing
said. `Session::load_from` warns on any clamp. The README currently documents
the asymmetry as it stands.

Two lines to make the global path warn on any clamp, matching the session path.
