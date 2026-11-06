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

## Partial downloads are opened through symlinks

Both download paths (`sftp.rs` and `ftp_impl.rs`, the `File::create(&part)`
and `.append(true)` opens) and `write_part_meta` (`transport/mod.rs`) use
plain `tokio::fs` opens, which follow a symlink at the target. The
`.blink-part` name and its `.meta` sidecar are predictable (a hash of the
file name), so anyone who can write to the download directory can plant
them ahead of time. A symlinked sidecar makes blink truncate the link's
target and write JSON into it. A symlinked partial with a matching
planted sidecar makes a resume append the download to the target.
`resume_offset`'s `metadata` call also follows the link.

This only matters for a download directory other users can write to
(`/tmp`, a shared drop folder); a home directory isn't one. Fix on Unix:
open with `custom_flags(libc::O_NOFOLLOW)` (or check `symlink_metadata`
first and refuse), and create a fresh partial with `create_new(true)`
after `resume_offset` removed the old one. `paths::atomic_write` already
uses `create_new`. Windows needs privilege to make symlinks, so `cfg(unix)`
is enough. Uploads have the same shape on the server
(`remote_part_path`, opened with CREATE|TRUNC); that's the server's
directory, so only note it there.

## The session list doesn't scroll

`render_session_list` (`tui/views.rs`) draws every session into one
`Paragraph` with no scroll offset, so sessions past the bottom of the
area are never drawn. The keyboard cursor still moves onto them, and the
selection disappears off-screen; only the details pane shows which
session is selected. How many fit depends on the terminal height. Mouse
clicks are consistent with what's drawn (`mouse_session_select` maps a
row to `sessions[r]`), so they'd need the same offset.

Fix the way the file panes do it: keep a `view_offset` that follows the
cursor (see `PaneState::view_offset` and its use in `mouse.rs`, "row `r`
is entry `view_offset + r`"), render only the visible slice, and add the
offset in `mouse_session_select`. A test with more sessions than rows
should check that the cursor's row is drawn.

## Session file names have no length cap

`Session::name_to_filename` (`session.rs`) has the problem
`checkpoint::file_name_for` had: it uses the whole name, so a session
name past 242 bytes produces a file name over the 255-byte limit
and `save` fails. Windows also refuses control characters, which the
name may contain (only NUL, CR and LF are rejected). Apply the same fix:
cut the stem on a char boundary to fit, and map control characters on
Windows only. Names that are too long can't have been saved, so nothing on
disk moves. The two functions then do the same thing, so they could share
one helper in `paths.rs`, with the hash input and suffix passed in.

## Checkpoints of an edited session are never cleaned up

Since checkpoints are keyed by origin (format 4), editing a saved
session's host, port or username leaves its old checkpoints behind:
they're never offered (wrong origin), but `is_orphaned`
(`checkpoint.rs`) only asks whether the session *name* still exists, so
`blink checkpoints --clean` keeps them. Only `--force` removes them, along
with everything else.

Fix: have `KnownSessions` hold (name, origin) pairs, built with
`transport::origin_of`, and count a checkpoint as orphaned when no saved
session has its name *and* origin. Ad-hoc sessions are unaffected:
their names were never saved, so their checkpoints already count as
orphaned.

## FTPS trust-on-first-use pins without showing the certificate

With `accept_invalid_certs = true`, the first connect pins whatever leaf
certificate it gets (`ftps.rs`, `PinningVerifier`, `None` arm), and the
log says only "pinned FTPS certificate for `name`" (`tui/app/events.rs`).
SFTP shows the host-key fingerprint and asks. Here the user has nothing
to compare out of band, so a man in the middle on the first connect is
pinned silently.

Cheapest step: put the SHA-256 in that log line. Fuller fix: prompt with
the fingerprint the way `HostKeyUnknown` does, and pin only on accept.
Also, during that first session the verifier accepts any certificate on
the *data* connections too, since `expected_pin` is still `None` there.
Those go to the control connection's peer, so this adds little beyond
the control connection itself, but a prompt-then-pin design should check
data connections against the captured pin.

## Encrypted keys are detected by matching error text

`SftpTransport::connect`'s `AuthMethod::Key` arm (`sftp.rs`) decides a key
needs a passphrase by lowercasing the error and looking for
"encrypted", "passphrase" or "decrypt". russh has a variant for the
missing-passphrase case, `russh::keys::Error::KeyIsEncrypted`; match that,
plus whatever a wrong passphrase returns (check against an encrypted
test key in `sftp_test_keys.rs`), and treat everything else as a load
error. Otherwise a wording change upstream silently breaks the
passphrase prompt.

## `Direction::CreateDir` reaches five `unreachable!()`s

`transfer::Direction` carries `CreateDir` for planned mkdirs, so every
match on a *batch's* direction (`checkpoint_glue.rs` ×3, `events.rs` ×2)
has a `Direction::CreateDir => unreachable!()` arm. None can be reached
today, but the type doesn't say so. Split out an upload/download enum for
batches (it maps one-to-one onto `CheckpointKind`, which may do), and
keep `CreateDir` only where jobs are executed.
