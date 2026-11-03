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

Last checked 2026-11-03: the fixes have landed upstream. #702 was closed
for the smaller RustCrypto/RSA#739 (blinding, merged 2026-11-01), and #680
for #749 (implicit rejection, merged 2026-11-01); #626 is closed.
`rsa 0.10.0-rc.20` was published 2026-11-03, after both merges, so it
should carry them — confirm against its changelog. What's left is
russh: 0.65.1 (blink is on 0.64.1) still pins `rsa =0.10.0-rc.18` and
`ssh-key =0.7.0-rc.11`. Once russh moves to rc.20 or later, check whether
RUSTSEC-2023-0071 names a patched version before dropping the ignore.

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

A related gap in the same function: the size check runs only when both
sides have a size. A sidecar with `size: None` (the server reported none
when the partial started) resumes on origin and path alone, even if the
server reports a size now. That contradicts the doc comment's "anything
unproven restarts". Make `None` on either side restart, for size now and
for mtime once it is added. That changes behaviour, not the format.

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

## `known_hosts` reads only part of the OpenSSH format

`check_in_str` (`known_hosts.rs`) splits each line on single spaces and
compares the first field to the host. The README calls the file "standard
OpenSSH format", but:

- `@revoked` and `@cert-authority` lines are skipped as non-matching, so
  a key marked revoked is never refused.
- Comma-separated host lists (`a,b ssh-ed25519 …`), hashed `|1|` hosts
  and tab-separated fields never match. These fail safe: the user gets
  a trust prompt.

blink writes its own file in the subset it reads, so this only bites
someone who pastes lines in from `~/.ssh/known_hosts`. Either honour
`@revoked` (refuse the key outright) and the other forms, or change the
README to say the file is a subset. `@revoked` is the one that matters.

## The legacy `host:port` known_hosts form is ambiguous for IPv6

`host_matches` (`known_hosts.rs`) also accepts `legacy_host`'s
`host:port`, so a line for `::1:22` matches host `::1` on port 22 and
host `::1:22` on port 22 (canonical form, default port). A key trusted
for one address then counts as trusted for the other. Hard to exploit,
but the fix is cheap: don't accept the legacy form when the host
contains `:`, or rewrite legacy lines to the canonical form on load.
Rewriting changes the file, so it needs Yukaia's OK first.

## Windows reserved names miss `CONIN$`, `CONOUT$` and superscript digits

`WINDOWS_RESERVED_STEMS` (`tui/plan.rs`) lists `CON` … `LPT9`, but
Windows also treats `CONIN$`, `CONOUT$`, and `COM¹`–`COM³` /
`LPT¹`–`LPT³` (U+00B9, U+00B2, U+00B3) as devices. A server can name a
download one of these, and on Windows it opens the device instead of
writing a file. Add them, and add cases to the
`safe_local_name_for(_, true)` tests. Windows only. The rules are
compiled on every host, so this can be checked here.

## Checkpoint file names have no length cap

`Checkpoint::path_for` (`checkpoint.rs`) builds
`{safe_name}-{hash8}-{kind}.json` from the full session name. A long
name goes past the 255-byte file-name limit, and the checkpoint can't
be written. `safe_name` also keeps control characters, which Windows
rejects in file names. Truncate `safe_name` (on a char boundary) so the
whole file name stays under 255 bytes; the hash suffix keeps truncated
names unique. Names that are already too long can't have a checkpoint,
so truncation moves nothing on disk. Mapping control characters to `_`
does rename the checkpoint of a session whose name has them, so do that
on Windows only, or let `offers_for` look under the old name too.
