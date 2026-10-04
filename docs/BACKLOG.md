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

Last checked 2026-10-04: russh 0.64.0 (blink is on 0.63.3) still pins
`rsa =0.10.0-rc.18` and `ssh-key =0.7.0-rc.11`; #626, #680 and #702 all
open, untouched since June.

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
