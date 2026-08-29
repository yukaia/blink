# Major Dependency Updates

**Date:** 2026-08-29  
**Status:** Approved  

## Summary

Five direct dependencies have new major versions that a lockfile sweep cannot reach: `russh` 0.60→0.63, `suppaftp` 8→10, `sha2` 0.10→0.11, `base64` 0.22→0.23, `icy_sixel` 0.5→0.6. Four of them need no code changes at all. `russh` needs two, one of which is a security decision about SSH host certificates.

The larger part of this work is not the upgrades. It is the FTP integration test harness that has to exist before the `suppaftp` bump means anything. FTP is the least-tested transport in the codebase: all six of its tests cover the path guard from `c8373c8`, and none exercise listing, transfer, or resume against a server. SFTP, by contrast, has a full in-process SSH+SFTP server behind fourteen tests. This spec closes that gap first and upgrades second.

## What was measured, not assumed

Each version was compiled against the current tree in a throwaway worktree rather than inferred from changelogs:

| Upgrade | Compile errors | Suite |
|---|---|---|
| suppaftp 8 → 10 | 0 | 410 pass |
| sha2 0.10 → 0.11 | 0 | 410 pass |
| base64 0.22 → 0.23 | 0 | 410 pass |
| icy_sixel 0.5 → 0.6 | 0 | 410 pass |
| russh 0.60 → 0.63 | 2 | — |

`suppaftp` 9.0's breaking changes were entirely the async-std→smol migration; blink is on the tokio runtime, so none apply. 10.0 changed `tcp_stream()` and `DataStream::into_tcp_stream` to return `FtpResult<TcpStream>`; blink calls neither.

## Scope

- **In scope:** an in-process FTP server harness with fault injection; ~12 FTP integration tests; the five version bumps; rejecting SSH host certificates; reconciling blink's CRLF path guard with suppaftp's new upstream one.
- **Out of scope:** `@cert-authority` host-certificate validation (rejected, not validated); an FTPS harness (wrapping the harness in rustls is a materially larger job — the TLS path keeps its three unit tests); live testing against real FTP daemons, so vsftpd/IIS quirks stay uncovered.

`russh-sftp` needs nothing: it is already at 2.4.0, the current release. It compiled unchanged against `russh` 0.63 in the probe — neither of the two errors touched it — so the SFTP layer does not move with the SSH layer.

## Architecture

### FTP test harness

New `#[cfg(test)] mod integration` in `src/transport/ftp_impl.rs`, mirroring the SFTP harness in `src/transport/sftp.rs:1301`.

```rust
/// Path -> file contents, shared by every connection to the server.
type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Protocol-level malformations the server emits on demand. suppaftp 10.0
/// converted these cases from panic to FtpError; nothing but a hand-rolled
/// server can produce them.
#[derive(Clone, Default)]
struct Faults {
    /// PASV reply carrying an out-of-range octet (999).
    bad_pasv_octet: bool,
    /// A LIST line that no parser can turn into an entry.
    unparsable_list_line: bool,
    /// Control response cut off mid-line.
    truncated_response: bool,
}

/// Binds :0, spawns the accept loop, returns the bound port and a counter of
/// accepted control connections (the dispatcher opens one per worker; the
/// counter is how reuse is asserted, as in the SFTP harness).
async fn start_server(store: Store, faults: Faults) -> (u16, Arc<AtomicUsize>);
```

The control connection is a line loop over exactly the commands blink issues, established by grepping the call sites:

`USER`, `PASS`, `TYPE`, `PASV`, `LIST`, `SIZE`, `REST`, `RETR`, `STOR`, `APPE`, `RNFR`, `RNTO`, `MKD`, `RMD`, `DELE`, `QUIT`.

`PASV` binds a fresh ephemeral listener and replies with the `(h1,h2,h3,h4,p1,p2)` tuple; the data connection is accepted, drained or filled, then closed to signal EOF. `REST` sets an offset that the next `RETR` honours, which is what makes download resume testable. Anything unrecognised gets `502 Command not implemented`, so a future blink change that starts issuing a new command fails loudly rather than hanging.

Data transfers are served in short slices, as the SFTP harness does, so partial reads are exercised rather than assumed away.

### Tests the harness enables

Roughly twelve, in `mod integration`:

1. Login, welcome message, `TYPE I` issued before any transfer.
2. `LIST` output parses into `FileEntry` values with the right names, sizes, and directory flags.
3. Download round-trip preserves bytes for a file larger than one transfer chunk.
4. Upload round-trip preserves bytes.
5. `REST`-based download resume continues from the stored offset and lands correct bytes.
6. `APPE`-based upload resume appends rather than truncating.
7. `SIZE` is consulted before transfer and its value reaches the progress reporting.
8. `rename`, `mkdir`, `rmdir`, `delete` each hit the right command and surface errors.
9. `QUIT` on a clean disconnect.
10. Fault: bad PASV octet yields a `BlinkError`, not a panic or hang.
11. Fault: unparsable LIST line yields a `BlinkError`, not a panic.
12. Fault: truncated response yields `Disconnected` within the timeout.

**These are written against suppaftp 8.0.5 and must pass there before the bump.** A harness written against the new version cannot distinguish "encodes current behaviour" from "encodes the new library's behaviour". Written against 8.0.5 first, it becomes a differential test: any behaviour change in step 2 surfaces as a failure instead of being silently absorbed.

### russh: host key or certificate

`russh` 0.63 widens the host-key callback:

```rust
// 0.60
async fn check_server_key(&mut self, key: &ssh_key::PublicKey) -> Result<bool, Self::Error>;
// 0.63
async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error>;
```

```rust
pub enum PublicKeyOrCertificate {
    PublicKey { key: PublicKey, hash_alg: Option<HashAlg> },
    Certificate(Certificate),
}
```

`KnownHostsHandler::check_server_key` (`src/transport/sftp.rs:107`) destructures it:

- **`PublicKey { key, .. }`** — the existing body verbatim, operating on `key`. Known-hosts lookup, TOFU prompt, session trust, and the sanitisation of algorithm names all stay exactly as they are.
- **`Certificate(_)`** — `tracing::warn!` and `Ok(false)`, following the fail-closed idiom already used when the known-hosts file cannot be read (`sftp.rs:150`). An `AppEvent` carries "host certificates are not supported" so the TUI shows the real reason instead of a generic connect failure.

Rejecting preserves today's behaviour: `russh` 0.60 never surfaced a certificate to this callback, so no connection that works now begins to fail. TOFU-pinning a certificate's key was rejected as a design because it ignores validity period, principals, and the CA signature while looking like verification.

The second site is the test server's `channel_open_session` (`src/transport/sftp.rs:1340`), which gains a fourth `ChannelOpenHandleInner<Msg>` parameter. Mechanical.

### suppaftp: guard reconciliation

`check_ftp_path` (`src/transport/ftp_impl.rs:187`) rejects `\r`, `\n`, and `\0` in remote paths. suppaftp 10.0.2 now rejects CR/LF at the library boundary as well.

blink's guard **stays as the primary**. It names the operation and produces a sanitised `BlinkError::transport`; suppaftp's would arrive as an opaque `FtpError`. The six existing guard tests are unchanged. The function's doc comment gains a line recording that upstream now backstops it, so a later reader does not delete ours as redundant.

### The three no-op upgrades

`sha2` 0.10→0.11, `base64` 0.22→0.23, `icy_sixel` 0.5→0.6: no code changes, one commit each so a regression bisects cleanly.

`sha2` 0.11 adds no duplicate. The tree already carries both 0.10.9 and 0.11.0 — `russh` reaches 0.10.9 through `bcrypt-pbkdf` and `ssh-encoding`, and 0.11.0 through `ed25519-dalek`, `p256`, and `p384`. This moves blink's own hashing onto the copy that is already compiled.

## Sequencing

Six commits, each independently revertible:

1. FTP harness + integration tests, against suppaftp 8.0.5.
2. suppaftp 8 → 10, behind that coverage; guard doc reconciliation.
3. russh 0.60 → 0.63: the two call sites, plus a test for the certificate arm.
4. sha2 0.10 → 0.11.
5. base64 0.22 → 0.23.
6. icy_sixel 0.5 → 0.6.

## Verification

Every commit gates on all four:

- `cargo test` — the full suite, currently 410 tests.
- `cargo clippy --all-targets` — zero warnings.
- `cargo audit --deny warnings` — exit zero.
- The `cargo metadata` MSRV walk — no package above 1.90.

The MSRV walk is a metadata check, not a compile. Only stable is installed in the development environment; a genuine MSRV guarantee needs `rustup toolchain install 1.90 && cargo +1.90 check`.

## Risks

- **The harness is a simplification.** It answers the way a well-behaved server does. Real daemons vary in `LIST` formats, `PASV` behaviour behind NAT, and TLS session reuse. The fault-injection switch covers malformation, not dialect. A manual smoke test against a real server before release remains worthwhile.
- **FTPS stays thinly covered.** The rustls path keeps its three unit tests. The FTPS-specific `AUTH TLS` upgrade and certificate pinning are not exercised end to end by anything here.
- **`russh` 0.63 may change behaviour beyond the two compile errors.** Nothing in the fourteen SFTP integration tests is expected to shift, but they are the evidence, and they run against an in-process server rather than a real `sshd`.
