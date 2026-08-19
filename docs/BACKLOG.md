# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `parallel_downloads` clamping is inconsistent between config and session

**Noted:** 2026-08-09, found by the 0.6.0 documentation audit.

`Config::load_from` warns only when the value is `0`, so a global
`parallel_downloads = 50` is silently reduced to the maximum of 10 with nothing
said. `Session::load_from` warns on any clamp. The README currently documents
the asymmetry as it stands.

Two lines to make the global path warn on any clamp, matching the session path.
