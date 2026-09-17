# Changelog

Notable changes to blink. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases before 0.7.0 predate this file and are not reconstructed here; see
the git history for those.

## [Unreleased]

### Security

- **`rustls` 0.23.43 -> 0.23.45, closing
  [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285)**
  (medium, 5.3) — TLS 1.3 handshake messages were accepted across encryption
  level boundaries. Reached through `suppaftp` and `tokio-rustls`, so it sat
  in the path of every FTPS connection. Lockfile only; the manifest never
  named `rustls` directly.

- **`wnaf` 0.14.0 -> 0.14.1, off a yanked release.** Reached through
  `russh -> p256/p384/p521 -> primeorder`. No advisory, but a yanked crate in
  the graph is a signal not to sit on.

### Changed

- **Semver-compatible sweep across the rest of the graph** (`cargo update`,
  40 packages). Notable: `russh` 0.63.1 -> 0.63.3, `clap` 4.6.6 -> 4.6.7,
  `smallvec` 1.15.2 -> 1.16.1, `syn` 3.0.4 -> 3.0.6. russh 0.63.3 drops its
  `internal-russh-num-bigint` fork for upstream `num-bigint` 0.5, taking the
  graph from 408 to 407 crates.

  Still deliberately out of scope, because each needs code changes:
  `russh-sftp` 2.4 -> 3.0, `suppaftp` 10.0 -> 12.0, `icy_sixel` 0.6 -> 0.7.

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
