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

Last checked 2026-10-09: russh 0.64.1 (blink is on it) still pins
`rsa =0.10.0-rc.18` and `ssh-key =0.7.0-rc.11`; #626, #680 and #702 all
open, untouched since June. `rsa 0.10.0-rc.19` is out, but with those
open it cannot carry the fix, and russh has not moved to it anyway.

## suppaftp: watch for a fix to cancelled data commands

blink no longer depends on this: an FTP connection now reconnects after
any call that breaks it. It is about the upstream side, and we are
waiting to see whether upstream fixes it on its own rather than filing.

Cancelling a data command after its data connection opens leaves
`data_connection_open` set, so every later data command fails with
`DataConnectionAlreadyOpen`. suppaftp#156 fixed the same flag for a data
command that *fails*, not one that is *cancelled*. In 12.1.1,
`data_command` sets the flag once the data socket connects
(`async_ftp/tokio_ftp.rs:1090`), and `data_command_with_response` clears
it only when the `150` read errors (1107–1116), so a future dropped while
awaiting the `150` leaves it set with no `TransferStream` to reset it.
`smol_ftp.rs` has the same code. Only `abort` is documented as not
cancellation-safe.

**What to check** at each suppaftp release: whether those two functions
changed — a drop guard, the flag set only after the `150`, or a docs note
that data commands need a reconnect once cancelled. Any of those closes
this; the harness fault `stall_next_retr` in `ftp_impl.rs` and
`a_preview_past_its_deadline_is_followed_by_a_reconnect` show the
behaviour. A fix changes nothing blink must do, since it reconnects
anyway.

Not reported upstream. If we change our minds, the issue to file
cites #155 and #156 and asks for one of the fixes above.

Last checked 2026-10-04: suppaftp 12.1.1, unchanged.

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

## Download resume can't detect a same-size replacement

`decide_resume` (`transport/mod.rs`) continues a `.blink-part` when the
sidecar's origin, remote path and reported size all match. A remote file
replaced by another of the same size between attempts passes all three,
so the result is the old file's head joined to the new file's tail. Nothing
reports this.

The fix is to also record the remote mtime in `PartMeta` and restart when
it differs or is missing. Adding the field is an on-disk format change to
the sidecar, so it needs Yukaia's OK first. A `#[serde(default)]` field
would leave old sidecars readable, but they would then restart rather
than resume. FTP reports mtime only through `MDTM` (or `MLST`), so check
what suppaftp gives before relying on it there.

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
