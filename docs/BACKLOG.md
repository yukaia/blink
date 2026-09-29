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

## Mouse support: follow-ups from its review

Found by the whole-branch review of mouse support (2026-09-28); all minor.

- **Two tests are weaker than their names.** In `src/tui/app/mouse.rs`,
  `double_clicking_the_parent_row_goes_up_and_transfers_nothing` checks
  `queue_counts()` straight after the clicks, but an upload only reaches the
  queue after its walk reports, so the check can't fail. And
  `a_click_with_a_filter_lands_on_the_filtered_entry_drawn_there` accepts
  any name containing `7` where it should require `file007`.
- **The wheel over the transfer list moves a highlight nobody can see.**
  The transfer cursor is only drawn while the bottom pane is focused
  (`render_transfers`), but the wheel moves it with a file pane focused,
  so after Tab the cursor, and what `c` cancels, is not where it was left.
- **Clicking the LOG body or the bottom pane's border does nothing.** A
  file pane focuses on a click anywhere in it; the bottom pane only
  answers on its tabs and transfer rows.
- **Two gestures have no test:** a click on a file pane's footer line, and
  a click after the listing shrank while the pane was scrolled (offset
  above zero). Both paths are correct by construction today.
- **Ctrl- and Alt-clicks act as plain clicks.** `wanted_mouse` ignores
  modifiers, so they move the cursor and can complete a double-click. A
  future selection gesture would need them free.
