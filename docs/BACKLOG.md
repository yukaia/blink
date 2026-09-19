# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `list` and `delete_dir` disagree about both-bits-set entries

`SftpTransport::delete_dir` (`src/transport/sftp.rs:689`) checks `is_symlink()`
*before* `is_dir()`, with a comment giving the reason: some SFTP servers report
a symlink-to-directory with both bits set, and recursing into one walks outside
the subtree the user named — possibly outside the connection's chroot.
`SftpTransport::list` (`src/transport/sftp.rs:1008`) checks them in the
opposite order, so the same entry comes back as `EntryKind::Directory`.

`walk_remote` skips on `EntryKind::Symlink` (`src/tui/plan.rs:228`), so it never
sees a symlink there and recurses into exactly the entry `delete_dir` refuses to
touch. The download planner is the unsafe side of the disagreement.

There is a third site, `SftpTransport::metadata`
(`src/transport/sftp.rs:1227`), which orders the checks the same way as `list`
but is *not* affected: it reads `russh_sftp`'s `metadata()`, which issues
`SSH_FXP_STAT` and so resolves the link server-side. Its `EntryKind::Symlink`
arm is close to unreachable. `list` reads `read_dir`, whose attributes are not
followed, which is why the order matters there and not here.

Not yet established: whether a real server actually sets both bits. The claim
lives only in `delete_dir`'s comment and no test or server is cited. Settle that
first — if it is theoretical, the ordering is harmless and this entry closes as
a comment fix.

Fix, if it is real: flip `list` to test `is_symlink()` first so both paths agree.
That also changes how a symlinked directory renders in the file pane, which is a
call worth making deliberately rather than as a side effect. The SFTP test
harness can list directories as of the delete-cap work, so this is now testable.

## The next round of major dependency updates

Three direct dependencies have majors that need code changes, and are held back
from the semver-compatible sweeps for that reason:

    russh-sftp   2.4.0  -> 3.0.0
    suppaftp     10.0.2 -> 12.0.0   (two majors)
    icy_sixel    0.6.0  -> 0.7.0

This wants a spec, not a backlog entry that grows — the 0.7.0 round got
`docs/superpowers/specs/2026-08-29-major-dependency-updates.md` and the same
shape applies. Two things carry over from that round. The FTP harness built to
land suppaftp 8 -> 10 differentially is still there and still the right tool for
10 -> 12, and the SFTP harness can now list directories too, so `russh-sftp`
3.0 has more to test against than 2.4 ever did. And `icy_sixel` renders through
a path no test can reach: the suite never rasterises sixel, so that upgrade ends
in a manual check the way 0.5 -> 0.6 did.

Watch `russh` rather than `ssh-key` and `rsa` directly: it pins both with exact
`=` requirements, so they move when it moves. Both are still pre-GA
(`ssh-key 0.7.0-rc.11`, `rsa 0.10.0-rc.18`), and `rsa` reaching a release is
what would retire the RUSTSEC-2023-0071 ignore in `.cargo/audit.toml`.

## Decide whether `cargo fmt` applies to this tree

`cargo fmt --check` fails with 308 hunks across 33 files. It is not whitespace
noise: the source is hand-wrapped narrower than rustfmt's default, so the tool
wants to re-flow real code. There is no `rustfmt.toml` and no formatting commit
anywhere in the history, so the question has never actually been answered.

Two honest options. Add a `rustfmt.toml` describing the wrapping the tree
already uses, so `cargo fmt` becomes a no-op and stays available as a check; or
record that formatting is deliberately manual and leave it. What should not
happen by accident is someone running `cargo fmt` and committing a 340-line
reflow across the whole tree, which is the current default outcome and would
flatten `git blame` over every file.
