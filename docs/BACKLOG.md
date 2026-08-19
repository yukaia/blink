# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `real_base_dir`'s environment validation has no test coverage

**Noted:** 2026-08-19, while making the config directory unreachable from tests.

The four platform `real_base_dir` implementations carry
`#[cfg_attr(test, allow(dead_code))]`, because `base_dir()` never calls them
under `cfg(test)`. Their validation — `$XDG_CONFIG_HOME` and `$HOME` must be
absolute, `%USERPROFILE%` must be set — is therefore never exercised, and
cannot be without `unsafe` env-var manipulation, which is also racy across
test threads.

Extracting a pure `base_dir_from(xdg: Option<&str>, home: Option<&str>)` that
takes the two values as arguments instead of reading them makes the logic
directly testable, leaving the env reads in a thin wrapper. Small, and the
validation is the part with the actual rules in it.

---

## `list_and_clean` decides and prints in one pass, so its policy is untested

**Noted:** 2026-08-19, while settling the `CheckpointStore` question.

`checkpoint::list_and_clean` (`src/checkpoint.rs:803`) is ~90 lines that walk
the checkpoint directory, decide what to remove, remove it, and print a report
— all in one function, with 13 `println!`/`eprintln!` calls inside what is
otherwise a domain module. It is called from exactly one place
(`src/main.rs:251`) and has no tests, because there is no way to observe what
it decided except by reading stdout.

The decision it makes is real logic:

```rust
let should_remove = force || (clean && (pending == 0 || orphaned));
```

`--force` takes everything; `--clean` takes checkpoints that are finished *or*
belong to no saved session. Nothing exercises that rule, or the label that
follows it (`forced` / `completed` / `orphaned`), and both are easy to get
subtly wrong when editing.

**The reason this is worth doing rather than merely tidy.** `orphaned` is
derived from

```rust
let known_sessions: HashSet<String> = Session::list_all().unwrap_or_default()
```

If `Session::list_all()` *fails* — an unreadable sessions directory, a
permissions problem — `unwrap_or_default()` yields an empty set, every
checkpoint then looks orphaned, and `--clean` silently escalates into
`--force`, deleting resumable checkpoints and sweeping the `.part` files of
transfers the user could still have finished. The command reports each one as
`(orphaned)`, which will look correct. No test would catch this today.

**Shape.** Split the decision from the doing: a pure function over
`(pending, orphaned, clean, force)` returning a disposition — remove with a
reason, or keep with a flag — leaving `list_and_clean` to map dispositions to
output. Then decide separately whether a failed `Session::list_all()` should
be a hard error rather than an empty set; treating "I could not read your
sessions" as "you have no sessions" is the actual defect, and the split is
what makes it testable.

---

## `parallel_downloads` clamping is inconsistent between config and session

**Noted:** 2026-08-09, found by the 0.6.0 documentation audit.

`Config::load_from` warns only when the value is `0`, so a global
`parallel_downloads = 50` is silently reduced to the maximum of 10 with nothing
said. `Session::load_from` warns on any clamp. The README currently documents
the asymmetry as it stands.

Two lines to make the global path warn on any clamp, matching the session path.
