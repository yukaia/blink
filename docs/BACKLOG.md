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

## A preview cancelled while its data connection opens can still wedge

suppaftp 12 recovers a transfer that is dropped once its `TransferStream`
exists. There is a window before that: if `timed_ftp`'s 60 s deadline
cancels `retr` or `list` inside suppaftp's `open_transfer`, the data socket
can already be open, with the client marked `data_connection_open`, but no
`TransferStream` has been made to leave a pending reply. The connection
then refuses data commands until reconnect. On the browsing connection
this needs a control channel that stalls for a minute mid-open, so it is
rare; it is upstream behaviour, not blink's. Worth a harness fault (stall
the `150` reply past the deadline) and an upstream report if it reproduces.
