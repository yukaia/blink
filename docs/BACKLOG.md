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

## `parallel_downloads` clamping is inconsistent between config and session

**Noted:** 2026-08-09, found by the 0.6.0 documentation audit.

`Config::load_from` warns only when the value is `0`, so a global
`parallel_downloads = 50` is silently reduced to the maximum of 10 with nothing
said. `Session::load_from` warns on any clamp. The README currently documents
the asymmetry as it stands.

Two lines to make the global path warn on any clamp, matching the session path.
