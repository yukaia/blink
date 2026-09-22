# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `ssh-key` and `rsa` reach a release

Watch `russh` rather than `ssh-key` and `rsa` directly: it pins both with exact
`=` requirements, so they move when it moves. Both are still pre-GA
(`ssh-key 0.7.0-rc.11`, `rsa 0.10.0-rc.18`), and `rsa` reaching a release is
what would retire the RUSTSEC-2023-0071 ignore in `.cargo/audit.toml`.

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

## Nothing tests the SFTP preview path

`SftpTransport::read_to_bytes` has no test: the SFTP harness covers
transfers, listing and deletes, not previews. russh-sftp 3.0 changed how
`File` reads (pipelined, 16 in flight), and the evidence that previews are
unaffected is a source reading alone. A test against the SFTP harness —
preview a file, preview one over the cap, list afterwards — would close
it, mirroring the FTP tests in `ftp_impl.rs`.
