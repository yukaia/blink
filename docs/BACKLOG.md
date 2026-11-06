# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `ssh-key` and `rsa` reach a release

Watch `russh` rather than `ssh-key` and `rsa` directly: it pins both with exact
`=` requirements, so they move when it moves. Both are still pre-GA
(`ssh-key 0.7.0-rc.11`, `rsa 0.10.0-rc.18`).

A release alone does not retire the RUSTSEC-2023-0071 ignore in
`.cargo/audit.toml`: the advisory lists no patched version, and names
rc.18 as affected. The fixes are RustCrypto/RSA#739 (blinding the default
decryption paths) and #749 (implicit rejection), which replaced #702 and
#680 and closed #626. Both merged 2026-11-01, and `rsa 0.10.0-rc.20`
(published 2026-11-03) should carry them — confirm against its changelog.
What's left is russh moving its pin to rc.20 or later; then check whether
RUSTSEC-2023-0071 names a patched version before dropping the ignore.

Last checked 2026-11-06: russh 0.65.1 is still the latest (blink is on
0.64.1) and still pins `rsa =0.10.0-rc.18` and `ssh-key =0.7.0-rc.11`.

## `Direction::CreateDir` reaches five `unreachable!()`s

`transfer::Direction` carries `CreateDir` for planned mkdirs, so every
match on a *batch's* direction (`checkpoint_glue.rs` ×3, `events.rs` ×2)
has a `Direction::CreateDir => unreachable!()` arm. None can be reached
today, but the type doesn't say so. Split out an upload/download enum for
batches (it maps one-to-one onto `CheckpointKind`, which may do), and
keep `CreateDir` only where jobs are executed.
