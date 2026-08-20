# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## Sanitize the remote name in the overwrite-confirmation modal

`src/tui/views.rs:1649-1650` renders `basename(remote_path)` into a
`Span::styled` with no `sanitize` call. It is the only place a server-supplied
name reaches the screen without one — the transfers pane has
`transfer_row_name_is_sanitized` and `transfer_row_name_strips_escape_sequences`
pinning the opposite policy a few files over.

Not an injection: `ratatui-core`'s `Buffer::set_stringn` filters
`char::is_control` and drops zero-width graphemes, and U+202E, U+200B, U+200E,
U+FEFF, U+2067 and U+061C all measure zero width, so none of them reach the
terminal. That filter was checked again on the 0.30 bump and is unchanged, so
this entry does not depend on the ratatui version. What survives
is that ratatui *deletes* those characters where blink *replaces them with a
space* — deliberately, per `is_deceptive_format`, whose doc comment gives the
reason as stopping a name that would "disguise a name the user already cleared
through the overwrite prompt". This modal is that prompt, and two distinct
remote names render identically in it.

Fix: wrap both arms in `crate::error::sanitize`. Test next to the transfers-pane
pair so the three render sites state one policy.

## Warn when FTP sends credentials in the clear

`src/transport/ftp.rs` issues `USER` / `PASS` and moves every byte over an
unencrypted socket, and nothing anywhere says so — not the connect log, not the
session-edit form, not the README. SFTP prompts on an unknown host key and FTPS
pins a certificate; plain `ftp://` is the one protocol that gives the user no
signal at all about what it is doing.

Fix: a `LogLevel::Warn` line on connect, and a line in the README's protocol
list. Consider a marker in the session selector next to `ftp` sessions. No
behaviour change — the user chose the protocol and can keep using it.

## `validate_theme_name` should reject `:` as well

`src/config.rs:223` rejects `/`, `\`, `\0` and `..`, but not `:`. On Windows a
path component carrying a drive prefix but no root replaces the whole buffer, so
`themes_dir().join("C:evil.ini")` resolves outside the themes directory
entirely. `safe_local_name_for` already documents and blocks exactly this
hazard for downloaded filenames; the two validators should agree.

Low reach — the name comes from the user's own `config.ini` or session file, not
from a server. Worth closing anyway because the asymmetry is the kind that gets
copied into the next validator.

## Cap the recursive remote delete

`SftpTransport::delete_dir` (`src/transport/sftp.rs:1073`) and
`ftp_delete_dir` (`src/transport/ftp_impl.rs`) grow their `Op` stack with no
ceiling. `walk_remote` guards the same shape with `MAX_QUEUED_JOBS` and returns
a real error naming the limit; the two delete walks never got the equivalent, so
a server serving a deep or wide enough tree exhausts memory instead.

No infinite-loop risk — symlinks are correctly treated as leaves in both walks.
Fix: reuse `MAX_QUEUED_JOBS` and the message `walk_remote` already produces.
