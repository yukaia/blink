# Dependency Majors, Round Two — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move `icy_sixel` 0.6→0.7, `russh-sftp` 2.4→3.0 and `suppaftp` 10→12, each behind a test that shows what the bump changes, and fix the FTP preview defect and error label that suppaftp 12 makes fixable.

**Architecture:** One commit per step so each dependency bisects on its own. Tests land before the bump they guard wherever they can pass on the old version (sixel round trip; preview relabel). The one test that cannot — the data-reset regression — is run on suppaftp 10 to record its failure and lands with the bump that fixes it.

**Tech Stack:** Rust 2024 (1.98), tokio 1.53, suppaftp (tokio + rustls-ring), russh-sftp, icy_sixel, image 0.25. Tests are in-module `#[cfg(test)]` (there is no `tests/` directory); FTP tests use the hand-rolled harness in `src/transport/ftp_impl.rs` `mod integration`.

**Spec:** `docs/superpowers/specs/2026-09-22-dependency-majors-round-two-design.md`

## Global Constraints

- Every commit passes all four gates (listed below, "Gates"). No commit leaves the tree failing.
- Run `cargo fmt` before every commit — formatting is rustfmt's defaults (`rustfmt.toml`, commit `09f59d5`) and nothing else enforces it.
- Manifest MSRV stays `rust-version = "1.98"`; no dependency may declare higher (checked: suppaftp 12.0.1 declares 1.88, the other two none).
- Exact target versions: `icy_sixel` 0.7.0, `russh-sftp` 3.0.0, `suppaftp` 12.0.1.
- Preview size-limit error text, exactly: `retr {remote_path}: file exceeds preview size limit`, as `BlinkError::Transport`.
- Work on branch `deps/round-two`; when done, fast-forward `main` locally. Do not push.
- Every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

### Gates

Run from the repo root; all four must be clean before each commit:

```bash
cargo fmt --check
cargo test -q 2>&1 | grep -E 'test result|FAILED|panicked'
cargo clippy -q --all-targets 2>&1 | head   # must print nothing
cargo audit -q                              # must exit 0
```

Test count at the start: **444**. Expected after Task 1: 445; after Task 4: 446; after Task 6: 447.

### Setup

```bash
cd /home/yukaia/distrobox/cargo/blink
git switch -c deps/round-two
```

---

### Task 1: Sixel round-trip test, on icy_sixel 0.6

**Files:**
- Modify: `src/preview.rs` — append a test to the existing `#[cfg(test)] mod tests` (it already has `use super::*;`)

**Interfaces:**
- Consumes (all private to `src/preview.rs`, reachable via `use super::*`):
  - `fn encode_png_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, image::ImageError>`
  - `fn scale_for_cells(image_bytes: &[u8], col: u16, row: u16, cols: u16, rows: u16) -> Result<ScaledForCells, PreviewError>`
  - `struct ScaledForCells { rgba: Vec<u8>, width_px: u32, height_px: u32, .. }`
  - `icy_sixel::SixelImage::from_rgba(Vec<u8>, usize, usize)`, `.encode() -> Result<String, _>`, `icy_sixel::SixelImage::decode(&[u8]) -> Result<SixelImage, _>` with pub fields `pixels: Vec<u8>` (RGBA), `width: usize`, `height: usize`. Same API on 0.6 and 0.7.
- Produces: test `sixel_output_decodes_back_to_the_image_it_was_given`, relied on by Task 2.

Why this shape: `cell_pixels()` reads the real terminal size when there is one, so the scaled dimensions vary between machines. The test therefore compares against whatever `scale_for_cells` returned, never against constants. Sixel encodes in six-pixel bands, so decoded height may be padded up to 5 rows; 0.6 and 0.7 decoders may differ in whether they pad, hence a range rather than an exact value.

- [ ] **Step 1: Write the test**

Append inside `mod tests` in `src/preview.rs`:

```rust
    /// The sixel backend is the one preview path whose output only a terminal
    /// could judge: kitty and iTerm2 carry PNG, which the suite can compare,
    /// but sixel is a palette-quantised escape stream. Decoding it back and
    /// bounding the error against what was encoded makes a broken or degraded
    /// encoder a test failure rather than a manual check after every
    /// `icy_sixel` bump.
    ///
    /// Compared against the *scaled* RGBA, not the source PNG, so this tests
    /// encoding and not the resize. The source mixes smooth gradients (which
    /// quantisation has to approximate) with hard edges (which it must not
    /// smear).
    #[test]
    fn sixel_output_decodes_back_to_the_image_it_was_given() {
        let (w, h) = (240u32, 160u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let edge = ((x / 20) + (y / 20)) % 2 == 0;
                rgba.extend_from_slice(&[
                    (x * 255 / w) as u8,
                    (y * 255 / h) as u8,
                    if edge { 220 } else { 30 },
                    255,
                ]);
            }
        }
        let png = encode_png_rgba(&rgba, w, h).unwrap();
        let scaled = scale_for_cells(&png, 0, 0, 400, 200).unwrap();

        let sixel = icy_sixel::SixelImage::from_rgba(
            scaled.rgba.clone(),
            scaled.width_px as usize,
            scaled.height_px as usize,
        )
        .encode()
        .unwrap();
        let decoded = icy_sixel::SixelImage::decode(sixel.as_bytes()).unwrap();

        let (sw, sh) = (scaled.width_px as usize, scaled.height_px as usize);
        assert_eq!(decoded.width, sw, "decoded width");
        assert!(
            decoded.height >= sh && decoded.height < sh + 6,
            "decoded height {} for a {sh}-row image; sixel pads to a six-row band at most",
            decoded.height,
        );

        let mut total = 0u64;
        for y in 0..sh {
            for x in 0..sw {
                let got = &decoded.pixels[(y * decoded.width + x) * 4..][..3];
                let want = &scaled.rgba[(y * sw + x) * 4..][..3];
                for c in 0..3 {
                    total += u64::from(got[c].abs_diff(want[c]));
                }
            }
        }
        let mean = total as f64 / (sw * sh * 3) as f64;
        assert!(
            mean < 10.0,
            "mean absolute error {mean:.2}/255 after a sixel round trip",
        );
    }
```

- [ ] **Step 2: Run it and read the real error value**

This is a characterisation test: it should pass on 0.6. To see the number it is bounding, temporarily change `mean < 10.0` to `mean < 0.0`, then run:

```bash
cargo test -q sixel_output_decodes_back 2>&1 | grep -E 'mean absolute error|test result'
```

Expected: FAIL, printing `mean absolute error X.XX/255`. Record X.XX for the commit message. **If X.XX is 8 or more, stop and report** — the bound would be too tight to survive Task 2.

Restore `mean < 10.0`.

- [ ] **Step 3: Run it for real**

```bash
cargo test -q sixel_output_decodes_back 2>&1 | grep 'test result'
```

Expected: `ok. 1 passed`.

- [ ] **Step 4: Gates, then commit**

Run the four gates. Expected count: 445.

```bash
cargo fmt
git add src/preview.rs
git commit -F - <<'EOF'
test(preview): round-trip the sixel encoder through a decoder

Sixel was the preview path only a terminal could check: kitty and iTerm2
carry PNG, but sixel is a palette-quantised escape stream, so the last
icy_sixel bump ended in a manual look. The encoder's output is now decoded
back with icy_sixel's own decoder and bounded against what went in: width
exact, height within the six-row band padding, mean RGB error under 10/255.

On icy_sixel 0.6 the mean is <X.XX>/255. Written on 0.6 so the 0.7 bump in
the next commit is measured by it, not the other way round.

444 tests -> 445.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

Replace `<X.XX>` with the value from Step 2 before committing.

---

### Task 2: icy_sixel 0.6 → 0.7

**Files:**
- Modify: `Cargo.toml` (the `icy_sixel = "0.6"` line), `Cargo.lock`

**Interfaces:**
- Consumes: Task 1's `sixel_output_decodes_back_to_the_image_it_was_given`.
- Produces: nothing new.

- [ ] **Step 1: Bump**

```bash
sed -i 's|^icy_sixel = "0.6"|icy_sixel = "0.7"|' Cargo.toml
cargo update -p icy_sixel
grep -A1 '^name = "icy_sixel"$' Cargo.lock
```

Expected: `version = "0.7.0"`.

- [ ] **Step 2: Read the new error value**

Repeat Task 1 Step 2's temporary `mean < 0.0` edit, run the same command, record the value (the spec measured a slight improvement on a different image, 6.13 → 5.95), restore `mean < 10.0`.

- [ ] **Step 3: Gates, then commit**

All four gates; count stays 445.

```bash
cargo fmt
git add Cargo.toml Cargo.lock
git commit -F - <<'EOF'
chore(deps): icy_sixel 0.6 -> 0.7

No code change. 0.7 corrects the DCS P1 -> pixel-aspect mapping to match
the VT340 reference, which only affects decoding: blink encodes, and both
versions emit the same `ESC P 9;1;0 q "1;1;w;h` header, square pixels. The
encoder's palette rounding changed, so output bytes differ.

The round-trip test from the previous commit passes unchanged: mean error
<X.XX>/255 on 0.6, <Y.YY>/255 on 0.7.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

Replace the placeholders with the values from Task 1 Step 2 and this task's Step 2.

---

### Task 3: russh-sftp 2.4 → 3.0

**Files:**
- Modify: `Cargo.toml` (the `russh-sftp = "2.0"` line), `Cargo.lock`

**Interfaces:** none. No code changes; the spec's "russh-sftp 3.0" section is the evidence.

- [ ] **Step 1: Bump**

```bash
sed -i 's|^russh-sftp = "2.0"|russh-sftp = "3.0"|' Cargo.toml
cargo update -p russh-sftp
grep -A1 '^name = "russh-sftp"$' Cargo.lock
grep -c '^name = "russh"$' Cargo.lock
```

Expected: `version = "3.0.0"`, and exactly `1` russh in the graph (3.0 asks for russh 0.63, which blink already uses).

- [ ] **Step 2: Gates, then commit**

All four gates; count stays 445.

```bash
cargo fmt
git add Cargo.toml Cargo.lock
git commit -F - <<'EOF'
chore(deps): russh-sftp 2.4 -> 3.0

No code change; nothing blink calls changed shape. Read as a source diff,
since the crate ships no changelog:

- Request timeouts are fixed at send rather than first poll. No change for
  blink: 2.4 awaited the timeout straight after sending, so the 10 s clock
  already started at send.
- A transport error now ends the receive loop and fails pending requests,
  where 2.4 logged it and left each request to time out.
- `File` pipelines reads (16 in flight) and allows 16 writes, up from 8.
  blink uses `File` only for previews; bulk transfers drive
  `RawSftpSession` with their own window.
- `io::ErrorKind::TimedOut` now becomes `Error::Timeout` rather than
  `Error::IO`; `map_sftp` sends both to `Disconnected`.
- `protocol/file_attrs.rs` is byte-identical, so `entry_kind` (480704c)
  classifies exactly as before.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 4: Preview size-limit error is a transport error, on suppaftp 10

**Files:**
- Modify: `src/transport/ftp_impl.rs`
  - `ftp_read_to_bytes` (currently around line 641)
  - harness `RETR` arm in `handle_control` (the `"RETR" => {` arm, around line 962)
  - new test in `mod integration`

**Interfaces:**
- Consumes (harness, in `mod integration`): `start_server(store: Store, faults: Faults) -> (u16, Arc<AtomicUsize>, Log)`, `test_session(port: u16) -> Session`, `with_timeout(fut, what: &str)`, `type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>`, `FtpTransport::connect(&Session, Option<&str>)`, `Transport::{read_to_bytes, list}`. `super::MAX_PREVIEW_BYTES: u64` (25 000 000).
- Produces: `ftp_read_to_bytes` returning `BlinkError::Transport("retr {path}: file exceeds preview size limit")` on overflow; harness `RETR` that answers `426` and keeps serving when its data write fails. Task 5 and 6 rely on the latter.

- [ ] **Step 1: Write the failing test**

Append inside `mod integration` in `src/transport/ftp_impl.rs`:

```rust
    /// Hitting the preview cap is not a dropped connection. It used to be
    /// reported as one — the cap was raised as `FtpError::ConnectionError`,
    /// which `map_ftp` classifies as `Disconnected` — and raising it from the
    /// `retr` callback also skipped finalisation, so on suppaftp 10 the
    /// connection refused every later data command. The preview runs on the
    /// browsing connection, so the next listing has to work.
    #[tokio::test]
    async fn an_oversized_preview_is_a_transport_error_and_the_connection_survives() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert(
                "/big.bin".to_string(),
                vec![7u8; super::MAX_PREVIEW_BYTES as usize + 1024],
            );
            files.insert("/small.txt".to_string(), b"hello".to_vec());
        }
        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let err = with_timeout(transport.read_to_bytes("/big.bin"), "oversized preview")
            .await
            .expect_err("a file over the cap must not preview");
        assert!(
            matches!(err, crate::error::BlinkError::Transport(_)),
            "the cap is not a disconnect: {err:?}",
        );
        assert!(err.to_string().contains("preview size limit"), "{err}");

        let entries = with_timeout(transport.list("/"), "list after the preview")
            .await
            .expect("the connection must still serve a listing");
        assert_eq!(entries.len(), 2);
        let small = with_timeout(transport.read_to_bytes("/small.txt"), "second preview")
            .await
            .expect("and a preview under the cap");
        assert_eq!(&small[..], b"hello");
    }
```

- [ ] **Step 2: Run it to see it fail**

```bash
cargo test -q an_oversized_preview_is_a_transport_error 2>&1 | grep -E 'the cap is not a disconnect|panicked|test result'
```

Expected: FAIL at `the cap is not a disconnect: Disconnected("retr /big.bin: Connection error: file exceeds preview size limit")`.

- [ ] **Step 3: Make the harness answer an early close the way a daemon does**

In `handle_control`'s `"RETR" => {` arm, replace:

```rust
                    for chunk in slice.chunks(DATA_SLICE) {
                        data.write_all(chunk).await?;
                    }
                    data.shutdown().await?;
                    drop(data);
                    w.write_all(b"226 transfer complete\r\n").await?;
```

with:

```rust
                    // A client may close the data connection before the body
                    // is sent — a preview stops reading at its size cap. A
                    // real daemon answers 426 and keeps serving; letting the
                    // write error escape would end the whole control
                    // connection instead, and whether it did would depend on
                    // how much of the body fit in the socket buffer.
                    let sent: std::io::Result<()> = async {
                        for chunk in slice.chunks(DATA_SLICE) {
                            data.write_all(chunk).await?;
                        }
                        data.shutdown().await
                    }
                    .await;
                    drop(data);
                    if sent.is_err() {
                        w.write_all(b"426 transfer aborted\r\n").await?;
                        continue;
                    }
                    w.write_all(b"226 transfer complete\r\n").await?;
```

Leave the `abrupt_close` branch above it untouched.

- [ ] **Step 4: Relabel the size-limit error**

Replace the whole of `ftp_read_to_bytes` with:

```rust
pub async fn ftp_read_to_bytes<T: TokioTlsStream + Send + 'static>(
    stream: &mut ImplAsyncFtpStream<T>,
    remote_path: &str,
) -> Result<Bytes> {
    check_ftp_path("retr", remote_path)?;
    let remote_path_owned = remote_path.to_string();
    // The cap is signalled out of band rather than as the callback's error.
    // An `Err` from the callback makes `retr` return without finalising the
    // transfer, which left the connection refusing every later data command
    // on suppaftp 10; and any `FtpError` it could carry maps to the wrong
    // thing — `ConnectionError` reads as a disconnect. So the callback always
    // returns `Ok`, `retr` finalises, and the flag decides the outcome.
    let over_cap = Arc::new(AtomicBool::new(false));
    let result = timed_ftp(
        "retr",
        remote_path,
        stream.retr(&remote_path_owned, {
            let over_cap = Arc::clone(&over_cap);
            move |reader| {
                // Cloned per call: `retr` takes an `FnMut`, so the closure
                // cannot give its own handle away to the future.
                let over_cap = Arc::clone(&over_cap);
                Box::pin(async move {
                    let mut buf = Vec::new();
                    let mut limited = reader.take(MAX_PREVIEW_BYTES + 1);
                    limited
                        .read_to_end(&mut buf)
                        .await
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    if buf.len() as u64 > MAX_PREVIEW_BYTES {
                        over_cap.store(true, Ordering::Relaxed);
                    }
                    Ok((buf, limited.into_inner()))
                })
            }
        }),
    )
    .await;
    // Checked before `result`: having stopped reading early, the server
    // usually answers 426, and that is not the error that happened.
    if over_cap.load(Ordering::Relaxed) {
        return Err(BlinkError::transport(format!(
            "retr {remote_path}: file exceeds preview size limit"
        )));
    }
    Ok(Bytes::from(result?))
}
```

Add to the top-level `use` block of `src/transport/ftp_impl.rs` (next to `use std::path::Path;`):

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
```

- [ ] **Step 5: Run the test**

```bash
cargo test -q an_oversized_preview_is_a_transport_error 2>&1 | grep -E 'panicked|test result'
```

Expected: `ok. 1 passed` — on suppaftp 10, confirming the spec's expectation that finalising removes the over-cap wedge. **If the listing or second preview fails, stop and report**: the spec's plan changes before anything builds on this.

(`mod integration` has its own `use std::sync::atomic::{AtomicUsize, Ordering}`; it is a separate module, so the new top-level imports do not clash with it.)

- [ ] **Step 6: Gates, then commit**

All four gates. Expected count: 446.

```bash
cargo fmt
git add src/transport/ftp_impl.rs
git commit -F - <<'EOF'
fix(ftp): report the preview cap as a transport error, not a disconnect

`ftp_read_to_bytes` raised "file exceeds preview size limit" from inside
the `retr` callback as `FtpError::ConnectionError`, so it surfaced as
`Disconnected("… Connection error: …")` although nothing had dropped. And
an `Err` from that callback makes `retr` skip finalisation: the transfer's
completion reply went unread, and the connection refused every later data
command with "Data connection is already open". The preview runs on the
browsing connection, so the remote pane stopped working until reconnect.

The callback now records the overflow in a flag and returns `Ok`, `retr`
finalises, and the flag is checked first. The server's 426 for the
abandoned body is not the error that matters. The message now reads
`retr <path>: file exceeds preview size limit`, as `BlinkError::Transport`.

This removes the over-cap trigger on suppaftp 10. A data-socket error
mid-preview still fails from inside the callback; suppaftp 12 handles that
one, two commits on.

The harness's RETR now answers 426 and keeps serving when the client closes
the data connection early, as a daemon does. Before, the write error ended
the whole control connection, and whether it did depended on how much of
the body fit in the socket buffer.

445 tests -> 446.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Task 5: Harness fault — a data connection reset mid-transfer

**Files:**
- Modify: `src/transport/ftp_impl.rs` — `struct Faults` and its doc comment, the `"RETR" => {` arm

**Interfaces:**
- Consumes: Task 4's `RETR` arm.
- Produces: `Faults::reset_data_after: Option<usize>`. Task 6's regression test uses it.

Why an RST, not a close: an orderly close reads as a short EOF, the callback returns `Ok`, `retr` finalises and reads the 426, and the connection is fine even on suppaftp 10. Only a read *error* makes the callback fail, and a reset is what produces one. `TcpStream::set_zero_linger()` (tokio 1.53, not deprecated — unlike `set_linger`) turns the close into an RST.

- [ ] **Step 1: Add the field**

In `struct Faults`, after `hostile_listing_names`, add:

```rust
        /// In RETR, send this many bytes, then reset the data connection
        /// (RST, not FIN), answer `426`, and keep serving control. A reset
        /// makes the client's read fail, where a clean close would read as a
        /// short EOF; only a failed read makes a `retr` callback return `Err`.
        pub reset_data_after: Option<usize>,
```

Update the doc comment above `#[derive(Clone, Default)]` from:

```rust
    /// Every field is read: `bad_pasv_octet` and `unparsable_list_line` by
    /// PASV/LIST below, `abrupt_close` by the abrupt-close branch of LIST,
    /// RETR and STOR/APPE alike, and `hostile_listing_names` by LIST.
```

to:

```rust
    /// Every field is read: `bad_pasv_octet` and `unparsable_list_line` by
    /// PASV/LIST below, `abrupt_close` by the abrupt-close branch of LIST,
    /// RETR and STOR/APPE alike, `hostile_listing_names` by LIST, and
    /// `reset_data_after` by RETR.
```

- [ ] **Step 2: Serve it in RETR**

In the `"RETR" => {` arm, directly after:

```rust
                    if faults.abrupt_close {
                        return Ok(());
                    }
```

insert:

```rust
                    if let Some(n) = faults.reset_data_after {
                        let _ = data.write_all(&slice[..n.min(slice.len())]).await;
                        data.set_zero_linger()?;
                        drop(data);
                        w.write_all(b"426 connection reset\r\n").await?;
                        continue;
                    }
```

- [ ] **Step 3: Gates, then commit (fault only)**

All four gates; count stays 446. Clippy must not flag the field as unused — Task 6's test is its first reader, but `Faults` derives `Default` and the field is read in `RETR`, so it is used.

```bash
cargo fmt
git add src/transport/ftp_impl.rs
git commit -F - <<'EOF'
test(ftp): harness fault for a data connection reset mid-transfer

`reset_data_after: Some(n)` makes RETR send n bytes, reset the data
socket with zero linger, answer 426, and keep serving control. A reset
rather than a close because only a failed read makes a `retr` callback
return `Err`, which is what left suppaftp 10's connection unusable; a
clean close reads as a short EOF and finalises normally.

Used by the regression test that lands with the suppaftp 12 bump.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

- [ ] **Step 4: Write the regression test (do not commit it)**

Append inside `mod integration`:

```rust
    /// A preview whose data connection is reset mid-read fails — that part is
    /// right — but on suppaftp 10 it also left the browsing connection
    /// refusing every later data command ("Data connection is already open"):
    /// the `retr` callback returned `Err`, so the transfer was never finalised
    /// and its reply never read. suppaftp 12's transfer streams leave that
    /// reply for the next command to drain, so the connection recovers.
    #[tokio::test]
    async fn a_preview_cut_by_a_data_reset_leaves_the_connection_usable() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/cut.bin".to_string(), pseudo_random(64 * 1024));
            files.insert("/small.txt".to_string(), b"hello".to_vec());
        }
        let faults = Faults {
            reset_data_after: Some(8192),
            ..Faults::default()
        };
        let (port, _c, _log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        with_timeout(transport.read_to_bytes("/cut.bin"), "reset preview")
            .await
            .expect_err("a reset data connection must fail the preview");

        let entries = with_timeout(transport.list("/"), "list after the reset")
            .await
            .expect("the connection must still serve a listing");
        assert_eq!(entries.len(), 2);
    }
```

- [ ] **Step 5: Record its failure on suppaftp 10**

```bash
cargo test -q a_preview_cut_by_a_data_reset 2>&1 | grep -E 'must still serve|panicked|test result' | tee /tmp/claude-1000/-home-yukaia-distrobox-cargo-blink/3f7f6da3-e9db-4413-8699-07d7d7c6edcf/scratchpad/reset-on-suppaftp-10.txt
```

Expected: FAIL at `the connection must still serve a listing` with an error containing `Data connection is already open`. Keep the file; Task 6's commit message quotes it.

**If it instead fails at `a reset data connection must fail the preview`** (the read saw EOF, not an error), or passes outright, stop and report: the fault is not producing a read error, and the regression test proves nothing.

Leave the test in the working tree, uncommitted. Task 6 commits it.

---

### Task 6: suppaftp 10 → 12

**Files:**
- Modify: `Cargo.toml` (the `suppaftp = { version = "10.0", …` line), `Cargo.lock`
- Modify: `src/transport/ftp_impl.rs` — `ftp_download` (around line 366), `ftp_upload` (around line 426); the Task 5 test is already in the working tree

**Interfaces:**
- Consumes: Task 5's uncommitted test `a_preview_cut_by_a_data_reset_leaves_the_connection_usable`; suppaftp 12's `TransferStream::finish(self) -> FtpResult<()>`.
- Produces: nothing new.

- [ ] **Step 1: Bump and see the two errors**

```bash
sed -i 's|suppaftp = { version = "10.0"|suppaftp = { version = "12.0"|' Cargo.toml
cargo update -p suppaftp
grep -A1 '^name = "suppaftp"$' Cargo.lock
cargo check --all-targets --message-format short 2>&1 | grep -E '^src.*error'
```

Expected: `version = "12.0.1"`; exactly two errors, `no method named finalize_retr_stream` (ftp_impl.rs ~369) and `no method named finalize_put_stream` (~426).

- [ ] **Step 2: Port the download**

In `ftp_download`, replace:

```rust
    timed_ftp(
        "finalize retr",
        remote_path,
        stream.finalize_retr_stream(reader),
    )
    .await?;
```

with:

```rust
    // Every early `?` above drops `reader` unfinished, and that is safe: a
    // dropped transfer leaves its completion reply for the next command to
    // drain, and the dispatcher closes a connection after any failed job
    // rather than pooling it. The preview path, which runs on the reused
    // browsing connection, is where an unfinished transfer ever mattered.
    timed_ftp("finalize retr", remote_path, reader.finish()).await?;
```

- [ ] **Step 3: Port the upload**

In `ftp_upload`, replace:

```rust
    timed_ftp("finalize put", &part, stream.finalize_put_stream(writer)).await?;
```

with:

```rust
    // As in `ftp_download`: early returns above drop `writer` unfinished,
    // which is safe because a failed job's connection is never reused.
    timed_ftp("finalize put", &part, writer.finish()).await?;
```

- [ ] **Step 4: Run the regression test**

```bash
cargo test -q a_preview_cut_by_a_data_reset 2>&1 | grep -E 'panicked|test result'
```

Expected: `ok. 1 passed`.

- [ ] **Step 5: Gates, then commit**

All four gates. Expected count: 447.

```bash
cargo fmt
git add Cargo.toml Cargo.lock src/transport/ftp_impl.rs
git commit -F - <<'EOF'
fix(ftp): suppaftp 10 -> 12, so a failed preview no longer wedges the connection

suppaftp 12 replaces `finalize_retr_stream` / `finalize_put_stream` with
a `TransferStream` that `finish()`es itself. One dropped without finishing
closes its socket and leaves its completion reply for the next command to
drain, so the control connection stays usable. On 10 a dropped transfer
left the client believing a data connection was still open.

That fixes the remaining preview defect. A preview whose data connection
fails mid-read returns `Err` from inside `retr`'s callback, which on 10
skipped finalisation and left the browsing connection refusing every later
data command. Measured with the reset fault from the previous commit, on
suppaftp 10:

    <paste the contents of reset-on-suppaftp-10.txt>

On 12 the preview fails and the next listing succeeds; that is the new
test.

Transfers change only mechanically. They drop unfinished streams on early
returns too, but the dispatcher never reuses a connection after a failed
job; a comment at each finish() says so.

11.0, also taken here, sends TLS close_notify before closing a download's
data stream. Strict TLS 1.3 FTPS servers answered the abrupt close with
"426 Transfer failed" after every byte had arrived. Nothing here exercises
FTPS end to end; that rests on upstream's tests.

446 tests -> 447.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

Replace the `<paste …>` line with the file's contents before committing.

---

### Task 7: CHANGELOG and backlog

**Files:**
- Modify: `CHANGELOG.md` — `## [Unreleased]`
- Modify: `docs/BACKLOG.md`

**Interfaces:** none.

- [ ] **Step 1: CHANGELOG — Fixed**

Under `## [Unreleased]` → `### Fixed`, after the existing last entry (the SFTP sockets/block-devices entry), add:

```markdown
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
```

- [ ] **Step 2: CHANGELOG — Changed and Dependencies**

In `### Changed`, replace the paragraph:

```markdown
  Still deliberately out of scope, because each needs code changes:
  `russh-sftp` 2.4 -> 3.0, `suppaftp` 10.0 -> 12.0, `icy_sixel` 0.6 -> 0.7.
```

with:

```markdown
  The three majors held back from this sweep are taken separately; see
  Dependencies.
```

Then add a new section after `### Changed` (the same heading the 0.7.0 release used):

```markdown
### Dependencies

- `russh-sftp` 2.4 -> 3.0, `suppaftp` 10 -> 12, `icy_sixel` 0.6 -> 0.7.
  Only suppaftp needed code, two lines. The sixel encoder is now checked by
  a round-trip test rather than by eye, so this bump did not end in a
  manual check.
```

- [ ] **Step 3: Backlog**

`docs/BACKLOG.md` has one entry, `## The next round of major dependency updates`. Its last paragraph (starting `Watch \`russh\` rather than \`ssh-key\` and \`rsa\` directly`) is still live and must survive. Replace the whole entry — from its `##` heading to the end of the file — with:

```markdown
## `ssh-key` and `rsa` reach a release

Watch `russh` rather than `ssh-key` and `rsa` directly: it pins both with exact
`=` requirements, so they move when it moves. Both are still pre-GA
(`ssh-key 0.7.0-rc.11`, `rsa 0.10.0-rc.18`), and `rsa` reaching a release is
what would retire the RUSTSEC-2023-0071 ignore in `.cargo/audit.toml`.
```

Before replacing, confirm the RC versions still hold:

```bash
grep -A1 -E '^name = "(ssh-key|rsa)"$' Cargo.lock
```

If either differs, use the version from `Cargo.lock` in the new entry.

The old entry's broken reference to `2026-08-29-major-dependency-updates.md` (the file is `…-design.md`) goes away with the entry, so there is nothing separate to correct.

- [ ] **Step 4: Gates, then commit**

All four gates; count stays 447.

```bash
git add CHANGELOG.md docs/BACKLOG.md
git commit -F - <<'EOF'
docs: changelog the dependency round, close its backlog entry

The majors entry is done. Its note on `ssh-key` and `rsa` is not — it is
what tracks retiring the RUSTSEC-2023-0071 ignore — so it moves to an
entry of its own rather than going with the rest.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
```

---

### Finish

- [ ] **Merge and report**

```bash
git switch main
git merge --ff-only deps/round-two
git branch -d deps/round-two
git fetch -q
git log --oneline -8
echo "ahead: $(git rev-list --count origin/main..main)"
```

Report the commit list and the ahead count. Do not push.
