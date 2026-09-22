# Dependency Majors, Round Two

**Date:** 2026-09-22  
**Status:** Approved  

## Summary

Three direct dependencies have majors the semver-compatible sweeps cannot
reach: `russh-sftp` 2.4→3.0, `suppaftp` 10→12, `icy_sixel` 0.6→0.7. Measured
against the tree, two need no code at all and the third needs two lines.

The work that matters is around them. `suppaftp` 12 fixes a defect blink has
today — an FTP preview that fails mid-read leaves the browsing connection
unusable until reconnect — so it lands with a regression test that fails on 10
and passes on 12. `icy_sixel` renders through a path no test reaches, which
made the last round's bump end in a manual check; a round-trip test closes that
gap before the bump rather than after. And the preview's size-limit error is
mislabelled as a disconnect, which is fixed alongside.

## What was measured, not assumed

Each bump was compiled and run alone in a throwaway worktree against `2e10e78`:

| Upgrade | Compile errors | Suite |
|---|---|---|
| russh-sftp 2.4.0 → 3.0.0 | 0 | 444 pass |
| suppaftp 10.0.2 → 12.0.1 | 2 | 444 pass after a two-line port |
| icy_sixel 0.6.0 → 0.7.0 | 0 | 444 pass |

Neither `russh-sftp` nor `icy_sixel` ships a changelog, so both were read as
source diffs. `suppaftp`'s is upstream in `CHANGELOG.md`.

### russh-sftp 3.0

No API blink uses changed. `protocol/file_attrs.rs` is byte-identical, so the
`entry_kind` classifier from `480704c` is unaffected. What moved is runtime
behaviour:

- **Request timeouts.** The deadline is now fixed when a request is sent rather
  than when its future is first polled. For blink that is no change: 2.4's
  `request()` awaited its timeout immediately after sending, so the 10 s clock
  already started at send. Pipelined reads are issued and awaited together.
- **A dead transport fails pending requests.** The receive loop now breaks on a
  transport error instead of logging and continuing, and dropping the session
  clears the request map. Previously each outstanding request waited out its
  own timeout. An improvement; nothing to change.
- **`File` reads are pipelined** (16 in flight by default; writes 8→16). blink
  uses `File` only in `read_to_bytes` for previews, and drives bulk transfers
  through `RawSftpSession` with its own window, so this touches only preview
  speed.
- `io::ErrorKind::TimedOut` now converts to `Error::Timeout` rather than
  `Error::IO`. `map_sftp` maps both to `Disconnected`, so classification is
  unchanged.
- The server `packet_len` limit check looked new in the diff but exists in 2.4
  as well.

### suppaftp 11 and 12

- **11.0** sends a TLS `close_notify` before closing a download's data stream.
  Strict TLS 1.3 FTPS servers answered an abrupt close with
  `426 Transfer failed` even after every byte had arrived. A real FTPS fix; no
  FTPS harness exists to show it, so it rests on upstream's tests.
- **12.0** replaces `finalize_retr_stream` / `finalize_put_stream` with a
  `TransferStream` returned by `retr_as_stream` / `put_with_stream`, finished by
  `TransferStream::finish(self)`. The stream shares the control connection
  rather than borrowing the client. Dropped without `finish()`, it closes its
  socket and records a pending reply that the *next* command drains before
  sending anything, so the control connection stays usable. While one is alive,
  opening another data connection fails with `DataConnectionAlreadyOpen`.
- 12.0.1 only exposes TLS connector traits. `rust-version` is 1.88, under
  blink's 1.98.

### icy_sixel 0.7

- The pixel-aspect mapping for DCS P1 was corrected to match the VT340
  reference. That affects decoding only: blink encodes, and both versions emit
  the same header, `ESC P 9;1;0 q "1;1;w;h`, square pixels.
- The encoder's palette rounding changed (e.g. `#0;2;87;12;86` →
  `#0;2;87;13;86`), so output bytes differ. Decoded against the source, a
  1200×800 gradient-with-edges image gives mean absolute error 6.13/255 on 0.6
  and 5.95/255 on 0.7. Release-build encode time is ~41 ms on both.

## The defect suppaftp 12 fixes

A probe against the FTP harness on 10.0.2 previewed an over-limit file, then
listed on the same connection:

```
preview of oversized file: Err(Disconnected("retr /big.bin: Connection error: file exceeds preview size limit"))
list on same connection:   Err(Transport("list /: Data connection is already open"))
small preview after:       Err(Transport("retr /small.txt: Data connection is already open"))
```

On 12.0.1 with the two-line port, the list and the second preview succeed.

The mechanism: `ftp_read_to_bytes` calls `retr` with a callback. When the
callback returns `Err`, suppaftp 10's `retr` returns without finalizing, so the
transfer's completion reply is never read and the client still believes a data
connection is open. Every later data command on that connection is refused.

The preview uses the TUI's browsing connection (`src/tui/app/viewer.rs`), which
has no reconnect on error, so the user's remote pane stops working until they
reconnect. Reachability is narrower than the probe suggests: `detect_view_kind`
refuses files whose *listed* size exceeds the limit, so the over-limit trigger
needs a listing that understates the size or a file that grew after listing. The
other trigger is any error reading the data socket mid-preview.

Transfers are not exposed. `ftp_download` and `ftp_upload` also drop their
stream unfinalized on an early `?`, but the dispatcher
(`src/transfer/dispatcher.rs`) closes a connection after any failed job instead
of pooling it, so a wedged connection is never reused.

## Scope

- **In scope:** the three bumps; a sixel round-trip test; relabelling the
  preview size-limit error; harness realism for an early-closed data connection;
  a harness fault for a reset data connection; the FTP regression test; closing
  the backlog entry.
- **Out of scope:** `abort()` on the preview error path (under 12 a dropped
  stream already recovers on the next command; `abort()` would save one round
  trip and add a failure mode of its own); an FTPS harness, so the 11.0
  `close_notify` fix stays covered only upstream; `russh` itself, already at the
  current 0.63.3.

## Design

### Sixel round-trip test

In `src/preview.rs`'s test module. Build an RGBA image in code — smooth
gradients plus hard edges, as the measurement used — and run it through the
same path `SixelBackend::render` uses: `scale_for_cells`, then
`SixelImage::from_rgba(..).encode()`. Decode the result with
`SixelImage::decode` and assert:

- decoded width equals the scaled width, and decoded height is the scaled height
  rounded up to the six-pixel sixel band;
- mean absolute RGB error over the scaled area is under **10/255**.

The bound sits above both measured values (6.13, 5.95) so a quantizer tweak
does not fail it, and far below what a broken encoder produces. It compares
against the scaled RGBA, not the original bytes, so it tests encoding and not
scaling. 0.6 has the same `decode` API, which is what lets the test land before
the bump.

### Harness realism: early-closed data connection

The harness's `RETR` writes the body with `data.write_all(chunk).await?`. If
the client closes the data socket early, the write fails and the `?` returns
from the whole control handler, killing the control connection. A real daemon
replies `426` and keeps serving. The harness is changed to do the same: on a
data write error, reply `426 transfer aborted` and continue the control loop.
`abrupt_close` keeps its own path and is untouched. No existing test depends on
the old behaviour; the earlier probe passed only because the remaining ~1 KB
fit in the socket buffer.

### Preview size-limit error

The callback in `ftp_read_to_bytes` currently turns "file exceeds preview size
limit" into `FtpError::ConnectionError`, which `map_ftp` classifies as
`Disconnected`. That is the wrong label, and returning `Err` from the callback
is also what triggers the wedge on 10.

The callback keeps reading up to `MAX_PREVIEW_BYTES + 1`, but on overflow it
sets a shared flag (`Arc<AtomicBool>`) and returns `Ok((buf, reader))`, so
`retr` finalizes normally. After `retr` returns, if the flag is set,
`ftp_read_to_bytes` returns
`BlinkError::transport("retr {path}: file exceeds preview size limit")`
whatever `retr` returned. The server usually answers `426` because reading
stopped early, and that reply is not the error that matters. The wording
matches the SFTP path.

This probably also removes the over-limit wedge on 10, since the callback no
longer fails. The test decides: if it does not pass on 10, the plan stops and
changes before anything builds on it.

### Harness fault: reset data connection

New `Faults` field, `reset_data_after: Option<usize>`. In `RETR`, send that many
bytes, then close the data socket with zero linger so the client sees a reset
(RST) rather than an orderly close, reply `426` on control, and keep serving.

An RST, not a FIN, because only a read *error* makes the preview callback fail.
An orderly close reads as a short EOF: the callback returns `Ok`, `retr`
finalizes, reads the `426`, and the connection stays healthy even on 10. How to
get zero linger on a tokio `TcpStream` is settled in the plan, since tokio's
`set_linger` may be deprecated.

### suppaftp 10 → 12

`ftp_download` and `ftp_upload` replace `stream.finalize_retr_stream(reader)` /
`stream.finalize_put_stream(writer)` with `reader.finish()` / `writer.finish()`,
still under `timed_ftp`. `ftp_read_to_bytes` compiles unchanged: its callback is
generic over the reader type. A comment at the transfer early returns records
why they need nothing more: the dispatcher never reuses a connection after a
failed job.

### Tests

| Test | Lands in | Passes on 10 | Passes on 12 |
|---|---|---|---|
| sixel round trip | step 1 | n/a (icy_sixel 0.6) | n/a (icy_sixel 0.7) |
| over-limit preview is `Transport`, names the limit, and a following `list` succeeds | step 4 | expected yes | yes |
| preview cut by a data reset fails, and a following `list` succeeds | step 6 | no: recorded in step 5 | yes |

The step-6 test is written in step 5 and run on 10 to capture the failure, and
the observed output goes in the step-6 commit message. It is not committed
failing: step 5 commits only the fault itself.

## Sequencing

Seven commits, each independently revertible:

1. Sixel round-trip test, on icy_sixel 0.6.
2. icy_sixel 0.6 → 0.7. The step-1 test passes unchanged.
3. russh-sftp 2.4 → 3.0. No code. The commit message records the behaviour
   notes above.
4. Harness `426` realism; preview size-limit relabel and its test. On suppaftp
   10.
5. Harness `reset_data_after` fault. The step-6 test is run here on 10 and its
   failure recorded, not committed.
6. suppaftp 10 → 12: the `finish()` port, the transfer-path comment, and the
   regression test.
7. CHANGELOG entries (the preview fix and relabel; the FTPS `close_notify`
   fix); close the backlog entry; correct the backlog's reference to the
   previous spec, which omitted `-design`.

## Verification

Every commit gates on:

- `cargo fmt --check`, per the formatting adopted in `09f59d5`;
- `cargo test`, currently 444 tests;
- `cargo clippy --all-targets`, zero warnings;
- `cargo audit`, exit zero with the existing `.cargo/audit.toml` ignore.

No new version declares a `rust-version` above the manifest's 1.98
(`suppaftp` 1.88; the other two declare none).

## Risks

- **The harness is still a well-behaved server.** The `426` realism and the
  reset fault model two failure shapes. Real daemons differ in when they send
  `426` versus `226` after an early close, and some send nothing until the next
  command. suppaftp 12's deferred-reply draining is what handles the
  variations, and it is tested upstream, not here.
- **FTPS stays thinly covered.** The 11.0 `close_notify` fix is the most
  user-visible change in the round and nothing here exercises it.
- **Sixel fidelity is a proxy.** A mean-error bound catches a broken or
  degraded encoder, not a terminal rendering it differently. What a terminal
  does with the stream stays a manual check, but only when the header or DCS
  parameters change, which this round they do not.
- **russh-sftp's pipelined `File` reads** change only preview fetch
  behaviour over SFTP, and nothing tests `SftpTransport::read_to_bytes`: the
  SFTP harness covers transfers, listing and deletes, not previews. The
  evidence for this part of 3.0 is the source reading alone. A preview test
  against the SFTP harness is cheap and would close it, but it is not in this
  round's scope.
