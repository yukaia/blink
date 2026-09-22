# Changelog

Notable changes to blink. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases before 0.7.0 predate this file and are not reconstructed here; see
the git history for those.

## [Unreleased]

### Added

- **Plain FTP now says that it is unencrypted.** Every other protocol tells
  the user something about how it protects them — an unknown SSH host key
  prompts, an FTPS certificate gets pinned — while `ftp://` protected nothing
  and said nothing about it. Connecting over FTP now logs a warning, and the
  README's protocol list and security notes say the same thing. The wording is
  anchored on the connection rather than the credential, so it stays true of
  an anonymous login: there is no password worth stealing there, but the file
  names and contents are in the clear either way.

  No behaviour change — the user chose the protocol and everything keeps
  working.

### Fixed

- **The session selector no longer shows FTPS as being as risky as FTP.**
  `Protocol::Ftp | Protocol::Ftps` shared one arm of the protocol-tag
  colouring, both rendering in the theme's warning colour. That was wrong in
  both directions: it cautioned against the encrypted protocol, and it gave
  no way to tell the cleartext one apart from it at a glance. FTPS now reads
  like SFTP, and the warning colour is reserved for protocols where
  `Protocol::is_cleartext()` holds.

- **SFTP no longer lists sockets and block devices as directories.** The
  entry type was read with `russh_sftp`'s `is_dir()` / `is_symlink()`, which
  test whether the mode *contains* a type's bits rather than whether the
  type field equals it — and the POSIX type codes overlap, so a socket
  (`0o140000`) and a block device (`0o060000`) both passed as directories.
  They showed as folders in the file pane, and a recursive download or delete
  tried to open them as one and failed. They are now `Other`: downloaded as
  files, unlinked as leaves. The directory listing, `metadata`, and the
  recursive delete now share one classifier rather than three copies of it.

- **A failed FTP preview no longer breaks the connection it ran on.**
  Previews use the browsing connection. When one failed partway — a file
  over the preview cap, or a data connection that dropped mid-read — the
  transfer was never finalised, and every later listing or preview on that
  connection was refused with "Data connection is already open" until you
  reconnected. The over-cap case is fixed in blink; the dropped-connection
  case by suppaftp 12, whose transfers finalise themselves.

- **Hitting the FTP preview cap is no longer reported as a disconnect.** It
  read `Connection error: file exceeds preview size limit` and was classed
  as a dropped connection. It is now a plain transport error:
  `retr <path>: file exceeds preview size limit`.

- **FTPS downloads from strict TLS 1.3 servers no longer fail at the end.**
  suppaftp 11 sends a TLS `close_notify` before closing a download's data
  connection; some servers answered the abrupt close with
  `426 Transfer failed` after every byte had arrived.

### Security

- **The recursive remote delete is bounded on both transports.**
  `SftpTransport::delete_dir` and `ftp_delete_dir` grew their walk stack with
  no ceiling, so a server serving a deep or wide enough tree exhausted memory
  instead of drawing an error. Both now stop at `MAX_QUEUED_JOBS` — the same
  ceiling, and the same reason, as `walk_remote` — and refuse before removing
  anything, rather than half-deleting the tree and then failing. Symlinks were
  already treated as leaves in both walks, so there was never an infinite-loop
  risk, only an unbounded one.

- **The overwrite-confirmation modal sanitizes the remote name.** It was the
  one place a server-supplied name reached the screen without it. Not an
  injection — `ratatui`'s `Buffer::set_stringn` drops control characters and
  zero-width graphemes before they reach the terminal — but ratatui *deletes*
  those characters where blink *replaces them with a space*, so two remote
  names differing only by a bidi override rendered identically in the very
  prompt the user clears them through. That is the case `is_deceptive_format`
  was written for.

- **`validate_theme_name` rejects `:`.** On Windows a path component carrying
  a drive prefix but no root replaces the whole buffer, so
  `themes_dir().join("C:evil")` resolved outside the themes directory
  entirely. `safe_local_name_for` already documented and blocked the same
  hazard for server-supplied download names. Low reach — a theme name comes
  from the user's own config, not from a server — but the asymmetry was the
  kind that gets copied into the next validator.

- **`rustls` 0.23.43 -> 0.23.45, closing
  [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285)**
  (medium, 5.3) — TLS 1.3 handshake messages were accepted across encryption
  level boundaries. Reached through `suppaftp` and `tokio-rustls`, so it sat
  in the path of every FTPS connection. Lockfile only; the manifest never
  named `rustls` directly.

- **`wnaf` 0.14.0 -> 0.14.1, off a yanked release.** Reached through
  `russh -> p256/p384/p521 -> primeorder`. No advisory, but a yanked crate in
  the graph is a signal not to sit on.

### Internal

- **The SFTP test harness can list directories.** Its `russh_sftp` handler
  implemented no directory operations at all, which left `delete_dir`, `list`
  and `mkdir` with no coverage on the SFTP side. It now serves `opendir`,
  `readdir` and `rmdir`, and refuses `rmdir` on a directory that still holds
  entries, so the walk's bottom-up order is enforced by the server rather than
  merely asserted. 430 tests -> 439.

- **RSA and ECDSA are covered, for host keys and client key auth.** The SFTP
  harness authenticated with a password only, so `AuthMethod::Key` had never
  run against a server, and the `rsa-sha2-512` negotiation the README lists
  as enforced was unreachable from any test. The harness now serves RSA and
  ECDSA host keys and verifies public-key auth; the keys are published test
  fixtures in `src/transport/sftp_test_keys.rs`, not secrets.

- **The sixel encoder is tested by a round trip.** Its output is decoded back
  and bounded against what was encoded (mean RGB error under 10/255), so an
  `icy_sixel` bump no longer ends in a manual look at a terminal.

- **The FTP harness answers like a daemon when a transfer is cut short.** An
  early-closed data connection now gets `426` with the control connection
  kept open, instead of ending the whole session; and a new fault resets the
  data connection mid-transfer, which is what exercises the preview fix
  above.

- **Formatting is rustfmt's defaults.** The tree was hand-wrapped and no
  configuration reproduced that wrapping, so it was reformatted once with a
  `rustfmt.toml` pinning the 2024 style. That commit is listed in
  `.git-blame-ignore-revs`; GitHub applies it automatically, and a local clone
  opts in with `git config blame.ignoreRevsFile .git-blame-ignore-revs`.
  Nothing enforces formatting, so run `cargo fmt` before committing.

- The suite stands at 447 tests.

### Changed

- **Semver-compatible sweep across the rest of the graph** (`cargo update`,
  40 packages). Notable: `russh` 0.63.1 -> 0.63.3, `clap` 4.6.6 -> 4.6.7,
  `smallvec` 1.15.2 -> 1.16.1, `syn` 3.0.4 -> 3.0.6. russh 0.63.3 drops its
  `internal-russh-num-bigint` fork for upstream `num-bigint` 0.5, taking the
  graph from 408 to 407 crates.

  The three majors held back from this sweep are taken separately; see
  Dependencies.

### Dependencies

- `russh-sftp` 2.4 -> 3.0, `suppaftp` 10 -> 12, `icy_sixel` 0.6 -> 0.7.
  Only suppaftp needed code, two lines. The sixel encoder is now checked by
  a round-trip test rather than by eye, so this bump did not end in a
  manual check.

### Documentation

- The README said some SFTP servers report a symlink with both the symlink
  and directory bits set. None can: the POSIX type codes make that
  combination impossible for a well-formed symlink. The Path-safety section
  now gives the real reason symlinks are leaves, and names sockets and
  devices alongside them.
- Third-party attributions corrected against each crate's own manifest.
  `webpki-roots` 1.x is CDLA-Permissive-2.0, not MPL-2.0; `icy_sixel` and
  `parking_lot` are MIT OR Apache-2.0, not MIT alone; `russh-sftp` is its
  own project (`AspectUnk/russh-sftp`), not part of `russh`; and `russh` and
  `icy_sixel` link to their current repositories.
- The viewer's scroll keys are documented, and the architecture tree lists
  `sftp_test_keys.rs`, so the private keys in the repository are explained
  where someone would find them.

### Known gaps

- FTPS still has no end-to-end test; the `close_notify` fix above rests on
  suppaftp's own tests.
- A preview cancelled while suppaftp is still opening its data connection can
  leave that connection refusing data commands until reconnect. It needs a
  control channel that stalls for a minute mid-open, and it is upstream
  behaviour; tracked in `docs/BACKLOG.md`.
- SFTP previews (`SftpTransport::read_to_bytes`) have no test; also in the
  backlog.

## [0.7.1] — 2026-08-29

### Fixed

- **`cargo build --target x86_64-pc-windows-gnu` failed on any machine without
  NASM.** `russh`'s default features pull `aws-lc-rs`, and `aws-lc-sys`
  assembles its Windows objects with NASM — a tool the README's MinGW
  instructions never mentioned. Linux takes a different assembly path, so a
  native build never noticed and the failure appeared only when
  cross-compiling. The Windows cross-compile now needs only `mingw-w64`.

### Changed

- **`russh` is built against `ring` rather than its default `aws-lc-rs`,**
  which removes `aws-lc-sys` from the dependency graph entirely (416 → 408
  crates) and is what fixes the Windows build above. The two are
  interchangeable implementations of the same SSH ciphers — russh's `ring` and
  `aws-lc-rs` branches import identical constants for AES-128/256-GCM and
  ChaCha20-Poly1305, and it refuses to compile with neither enabled. `ring`
  was already being built for TLS through `rustls`, so blink now compiles one
  crypto backend instead of two.

  Note the SFTP integration harness generates only Ed25519 keys, so the test
  suite does not exercise RSA or ECDSA against the swapped backend; the
  feature-level parity above is the evidence, not the test run.

  A test reads `Cargo.lock` and fails if `aws-lc` returns. russh only rejects
  *neither* backend being enabled, so any dependency that switches
  `aws-lc-rs` back on through feature unification would win silently — and
  break a target nobody builds daily.

- **Minimum supported Rust declared as 1.98**, replacing the 1.90 shipped in
  0.7.0. Both 1.89 and 1.90 were metadata claims: a declared `rust-version`
  records what a crate says about itself, not what it compiles at, and nothing
  here has ever been built below stable. 1.98 is the only floor anyone has
  verified. It is likely higher than the true floor, which is the honest
  direction to be wrong in — lowering it needs a real
  `cargo +<version> check --all-targets`, not another metadata walk.

### Documentation

- Added this changelog, starting at 0.7.0.
- Recorded that SSH host certificates are refused, and that FTP listings are
  parsed as POSIX or DOS only, in the feature list, security notes and
  caveats.
- Credited `ring`, now the crypto behind every connection, under its actual
  Apache-2.0 AND ISC license. Corrected the `tokio-rustls` and `tokio-util`
  credit rows: both were dropped as direct dependencies in 0.7.0 but still
  ship transitively, through `suppaftp` and `russh-sftp`.
- Corrected the `.cargo/audit.toml` rationale for RUSTSEC-2023-0071, which
  named a dependency path through russh's `internal-russh-forked-ssh-key`
  fork that russh 0.63 had already dropped.

## [0.7.0] — 2026-08-29

### Security

- **FTP control-channel injection via remote paths.** Every path blink sent
  came from a server listing or a session file, and nothing between there and
  the wire looked for a line terminator. suppaftp builds commands as
  `format!("RETR {p}")` and appends CRLF without escaping, so a path carrying
  CR or LF stopped being a path and became a second command. The case that
  matters is a shared server, where the sender is not the operator: another
  tenant names a file `x\r\nDELE //…`, and it fires the moment the victim
  lists or downloads that directory, executed with the victim's credentials.
  Remote paths carrying CR, LF, or NUL are now refused at the FTP boundary,
  with the operation named and the offending bytes sanitised out of the error.

- **The same injection reachable through a session URL.** `Session::from_url`
  validated `host` and `username` but never called `validate()`, which had
  always checked `remote_dir` — so the one field a session *file* could not
  carry was reachable through the URL that builds a session without one. The
  path is percent-decoded on the way in, so
  `ftp://user@host/pub%0D%0ADELE%20%2Fimportant.txt` produced a session whose
  `remote_dir` was `/pub\r\nDELE /important.txt`, sent on the first listing.

### Fixed

- **FTPS panicked on every connect.** `tokio-rustls` was declared with default
  features, which enabled rustls' `aws-lc-rs` backend alongside the `ring`
  backend already selected through suppaftp. rustls 0.23 refuses to guess
  between two providers, so `ClientConfig::builder()` panicked outright and
  the FTPS transport was unusable. blink now routes through suppaftp's own
  `rustls` re-export, so exactly one provider can reach rustls.

- **A failed `RETR` left `REST`'s offset set,** so the next download silently
  started mid-file and wrote a corrupt result. RFC 959 has the restart marker
  consumed by the next transfer command regardless of outcome.

- **Unparsable FTP listing lines became browser entries.** suppaftp's
  `File::from_str` falls back to the MLSX parser, which splits on `;` and
  names the file after the trailing token — so *any* non-empty line parsed, as
  a file named after the whole line, addressing nothing. blink issues `LIST`
  and never `MLSD`/`MLST`, so it now parses with the POSIX and DOS parsers
  only and skips what neither accepts.

- **`--clean` deleted checkpoints it could not vouch for.** Orphanhood was
  decided from `Session::list_all().unwrap_or_default()`, which read "I could
  not list your sessions" as "you have no sessions" — every checkpoint then
  looked orphaned and `--clean` quietly did the job of `--force`, sweeping
  resumable batches and their `.part` files while reporting each as
  "(orphaned)". A single unparseable `.ini` was enough to trigger it.

- **`parallel_downloads` had an accidental cliff at 255.** Both loaders parsed
  into `u8`: below the cliff, out-of-range values clamped; above it, the parse
  failed, so `parallel_downloads = 300` stopped blink starting from
  `config.ini` and was silently ignored in a session file. Both now parse into
  `u32` and clamp to the documented cap.

### Changed

- **SSH host certificates are refused, fail-closed.** russh 0.63 widened the
  host-key callback to surface certificates as well as keys. `known_hosts`
  maps a host to a literal key and blink has no `@cert-authority` support, so
  nothing can validate a certificate's CA signature, principals, or validity
  window; pinning one by its key would look like verification while checking
  none of that. The rejection reason is surfaced to the TUI rather than left
  as a generic connect failure. russh 0.60 could not surface a certificate to
  that callback at all, so no connection that worked before begins to fail.

- **Unparsable listing lines are now reported.** One `skipped N of M
  unparsable lines` warning per listing — aggregated per call rather than per
  line, because listing runs on interactive navigation.

- **Minimum supported Rust raised to 1.90.** The manifest declared 1.89 but
  had never been buildable at it: `quantette`, reached through `icy_sixel`,
  declares 1.90 and every published release does. (Superseded in 0.7.1 — see
  below.)

### Dependencies

- suppaftp 8 → 10, russh 0.60 → 0.63, sha2 0.10 → 0.11, base64 0.22 → 0.23,
  icy_sixel 0.5 → 0.6, ratatui → 0.30.2, crossterm → 0.29.
- Moved off the yanked chacha20 0.10.1; dropped the unused `tokio-util`
  dependency.
- russh 0.63 drops its `internal-russh-forked-ssh-key` fork for upstream
  `ssh-key`.

### Internal

- **An in-process FTP server harness with fault injection**, and roughly
  twenty integration tests over it. FTP was previously the least-tested
  transport: all six of its tests covered the path guard, and none exercised
  listing, transfer, or resume against a server. The harness was built and
  landed *before* the suppaftp bump so that upgrade could be measured rather
  than assumed — a malformed PASV octet panicked under 8.0.5 and returns an
  error under 10.0.2, observed on both sides.
- Config-directory test isolation, so tests can no longer reach or adopt the
  real user configuration.
- Test count 410 → 426.

### Known gaps

- FTPS has unit-test coverage only; `AUTH TLS` and certificate pinning are not
  exercised end to end.
- The FTP harness models a well-behaved server. Real-daemon dialects
  (vsftpd, IIS), NAT-mangled PASV replies, and TLS session reuse are uncovered.
- The MSRV floor is checked by a `cargo metadata` walk over declared
  `rust-version` fields, not by compiling at that version.
