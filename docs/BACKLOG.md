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
rc.18 as affected. What would close it is a `rsa` release carrying
RustCrypto/RSA#680 (implicit rejection) or #702 (blinding the default
decryption paths), which track RustCrypto/RSA#626. Check those first, then
whether russh has moved its pin to that release.

Last checked 2026-11-03: the fixes have landed upstream. #702 was closed
for the smaller RustCrypto/RSA#739 (blinding, merged 2026-11-01), and #680
for #749 (implicit rejection, merged 2026-11-01); #626 is closed.
`rsa 0.10.0-rc.20` was published 2026-11-03, after both merges, so it
should carry them — confirm against its changelog. What's left is
russh: 0.65.1 (blink is on 0.64.1) still pins `rsa =0.10.0-rc.18` and
`ssh-key =0.7.0-rc.11`. Once russh moves to rc.20 or later, check whether
RUSTSEC-2023-0071 names a patched version before dropping the ignore.

## Checkpoints don't record which server and account they came from

Checkpoints are keyed only by session name (`Checkpoint::path_for` in
`checkpoint.rs`), and `Session::from_url` names an ad-hoc session after
the bare host. So `sftp://alice@host` and `sftp://bob@host` share a
checkpoint, and so do a saved session and a session renamed or edited to
point at another server. Resuming an upload offered from one of these
sends the first account's files to the second.

The fix is to store `origin_of(session)` (`transport/mod.rs`, already used
by the `.blink-part` sidecar) in the checkpoint, and have `offers_for` and
resume skip or flag a checkpoint whose origin doesn't match. That is an
on-disk format change, so it needs Yukaia's OK first. Still to decide:
what to do with existing checkpoints that have no origin. The suggestion
was to keep offering them, with a warning that blink can't confirm the
server.

## Share the SSH agent login loop between Unix and Windows

The `AuthMethod::Agent` arm in `transport/sftp.rs` has two copies of the
same identity loop: an inline one under `#[cfg(unix)]`, and
`try_agent_identities` under `#[cfg(windows)]`. They have already drifted.
Unix reports the last rejection error and Windows only logs it. The
"no identities" hints also differ.

Make `try_agent_identities` cross-platform, and have both branches call it
so they report errors the same way. Held back because the Windows build
can't be checked here: do it where `cargo check --target
x86_64-pc-windows-msvc` (or a Windows CI run) can confirm the result.
