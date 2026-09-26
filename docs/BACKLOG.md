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

Last checked 2026-09-26: russh 0.63.3, and its `main`, still pin rc.18;
#626, #680 and #702 all open, untouched since June.

## suppaftp: one issue to file, one fix to pick up

blink no longer depends on either: an FTP connection now reconnects after
any call that breaks it, and `map_ftp` maps `BadResponse` to
`Disconnected`. These are about the upstream side.

**To file.** Cancelling a data command after its data connection opens
leaves `data_connection_open` set, so every later data command fails with
`DataConnectionAlreadyOpen`. suppaftp#156 fixed the same flag for a data
command that *fails*, not one that is *cancelled*. Not reported upstream as
of 2026-09-26. The harness fault `stall_next_retr` in `ftp_impl.rs`
reproduces it. What to file: `data_command` sets the flag once the data
socket connects, and only a failed `150` clears it, so a future dropped
while awaiting the `150` leaves it set with no `TransferStream` to reset it.
Ask for a guard that clears it on drop, or a docs note that data commands
need a reconnect after cancelling. Link #155 and #156.

**To pick up.** suppaftp#184 (issue #183) makes a control connection that
closes before its reply a `ConnectionError(UnexpectedEof)` instead of
`BadResponse`. It was merged on 2026-09-25, after 12.1.0, and is
unreleased. At the release that carries it, bump suppaftp. The mapping
stays correct, but the `map_ftp` doc comment in `error_map.rs` that says
suppaftp reports a closed connection as `BadResponse` needs rewording, and
`a_dropped_control_connection_is_followed_by_a_reconnect` then passes
through `ConnectionError` instead.

## An unparsable FTP listing says nothing in the TUI

A server whose `LIST` lines neither the POSIX nor the DOS parser accepts
shows an empty directory. `ftp_list` (and `ftp_metadata`) in
`ftp_impl.rs` report the skipped lines only through `tracing::warn!`,
which is discarded unless `BLINK_LOG_FILE` is set, so the user sees an
empty pane and no reason. The comment above the warning says silence was
the problem it fixed; it only fixed it for someone reading the debug log.

The FTP transports have no channel to the TUI log: the SFTP transport
gets an `AppEvent` sender for host-key prompts, FTP gets nothing, and
the plain-FTP warning is pushed by the app on connect, not by the
transport. Either give the FTP transports a sender, or have `list`
report the skip count to its caller (a `Transport` interface change,
so SFTP would return zero). One log line per listing, as now, not one
per skipped line. `an_unparsable_listing_line_becomes_no_entry_at_all`
in the FTP harness is the place to assert it.
