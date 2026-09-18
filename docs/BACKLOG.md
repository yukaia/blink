# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## Warn when FTP sends credentials in the clear

`src/transport/ftp.rs` issues `USER` / `PASS` and moves every byte over an
unencrypted socket, and nothing anywhere says so — not the connect log, not the
session-edit form, not the README. SFTP prompts on an unknown host key and FTPS
pins a certificate; plain `ftp://` is the one protocol that gives the user no
signal at all about what it is doing.

Fix: a `LogLevel::Warn` line on connect, and a line in the README's protocol
list. Consider a marker in the session selector next to `ftp` sessions. No
behaviour change — the user chose the protocol and can keep using it.

Note the transports hold no logger: `grep LogLevel src/transport/` returns
nothing, and the host-certificate rejection reached the log by way of an
`AppEvent` instead. The natural seam is `AppEvent::Connected`
(`src/tui/app/events.rs:34`), which has the session and so the protocol, and
which already carries the FTPS pin messages.

## `list` and `delete_dir` disagree about both-bits-set entries

`SftpTransport::delete_dir` checks `is_symlink()` *before* `is_dir()`
(`src/transport/sftp.rs`), with a comment giving the reason: some SFTP servers
report a symlink-to-directory with both bits set, and recursing into one walks
outside the subtree the user named — possibly outside the connection's chroot.
`SftpTransport::list` checks them in the opposite order, so the same entry
comes back as `EntryKind::Directory`.

`walk_remote` skips on `EntryKind::Symlink` (`src/tui/plan.rs:228`), so it never
sees a symlink there and recurses into exactly the entry `delete_dir` refuses to
touch. The download planner is the unsafe side of the disagreement.

Not yet established: whether a real server actually sets both bits. The claim
lives only in `delete_dir`'s comment and no test or server is cited. Settle that
first — if it is theoretical, the ordering is harmless and this entry closes as
a comment fix.

Fix, if it is real: flip `list` to test `is_symlink()` first so both paths agree.
That also changes how a symlinked directory renders in the file pane, which is a
call worth making deliberately rather than as a side effect. The SFTP test
harness can list directories as of the delete-cap work, so this is now testable.
