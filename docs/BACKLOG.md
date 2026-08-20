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

Not an injection: ratatui 0.29's `Buffer::set_stringn` filters `char::is_control`
and drops zero-width graphemes, and U+202E, U+200B, U+200E, U+FEFF, U+2067 and
U+061C all measure zero width, so none of them reach the terminal. What survives
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

## Bump ratatui to 0.30 to clear three advisories

`cargo audit` reports no vulnerabilities but three warnings, all transitive
through ratatui 0.29:

| ID | Crate | Kind |
|---|---|---|
| RUSTSEC-2026-0002 | lru 0.12.5 | unsound — `IterMut` violates Stacked Borrows |
| RUSTSEC-2026-0253 | lru 0.12.5 | unsound — UAF via panic in `LruCache::pop()` |
| RUSTSEC-2024-0436 | paste 1.0.15 | unmaintained |

Neither `lru` issue sits on an attacker-reachable path — it backs ratatui's
internal layout cache. The reason to act is `.cargo/audit.toml`'s standing rule
that every warning carries a written rationale: these three have none, and an
unexplained warning is how the whole signal starts getting ignored.

Checked against 0.30.2: it pulls `lru 0.18.2` and no `paste` at all, so the bump
clears all three rather than needing three ignore entries. It also keeps a
`crossterm_0_28` feature, so the crossterm 0.28 pin does not have to move in the
same change — select it explicitly, since the default `crossterm` feature
resolves to 0.29 and enabling both pulls in two copies.

The cost is the 0.30 split into `ratatui-core` / `ratatui-widgets` /
`ratatui-crossterm` and whatever import churn that implies. If it turns out to
be more than churn, this wants a spec rather than a backlog entry.
