# Major Dependency Updates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an in-process FTP test harness, then land five major dependency upgrades behind real coverage.

**Architecture:** A hand-rolled FTP server in `#[cfg(test)]` code, mirroring the existing in-process SSH+SFTP harness, backed by an in-memory `HashMap` store and carrying a fault-injection switch for malformed protocol responses. The harness is written and landed against the *current* suppaftp 8.0.5 so it reads as a differential test; only then does the dependency move. Four of the five upgrades need no source changes at all.

**Tech Stack:** Rust 2024, tokio, suppaftp (FTP/FTPS), russh + russh-sftp (SSH/SFTP), ratatui.

**Spec:** `docs/superpowers/specs/2026-08-29-major-dependency-updates-design.md`

## Global Constraints

- **MSRV is 1.90.** Do not introduce a dependency declaring a higher `rust-version`.
- **No new runtime dependencies.** The harness uses only `tokio` and `std`, both already present. No new `[dev-dependencies]` either.
- **Every commit gates on all four:** `cargo test` (410 passing at plan time, growing as tasks land), `cargo clippy --all-targets` with zero warnings, `cargo audit --deny warnings` exiting zero, and the MSRV metadata walk showing no package above 1.90.
- **Never `git push`.** Commit to `main` locally and report how many commits ahead the branch is. Pushing is the user's own step.
- **Test naming follows the codebase:** full sentences as function names (`a_partial_of_this_file_is_resumed_and_completes_correctly`), not `test_foo`.
- **Sanitisation is load-bearing.** `RemoteEntry::new` derives `display_name` via `crate::error::sanitize`. Never construct a `RemoteEntry` literal in transport code.

### The MSRV metadata walk

Used as a gate throughout. Save as `/tmp/msrv.py`:

```python
import json, subprocess
FLOOR = (1, 90)
md = json.loads(subprocess.run(
    ["cargo", "metadata", "--format-version", "1"],
    capture_output=True, text=True).stdout)
ids = {n["id"] for n in md["resolve"]["nodes"]}
bad = []
for p in md["packages"]:
    if p["id"] not in ids or not p.get("rust_version"):
        continue
    if tuple(int(x) for x in p["rust_version"].split(".")[:2]) > FLOOR:
        bad.append((p["name"], p["version"], p["rust_version"]))
for n, v, rv in sorted(bad):
    print(f"  {n} {v} requires rust {rv}")
print(f"TOTAL over 1.90: {len(bad)}")
```

Expected output at every gate: `TOTAL over 1.90: 0`.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/transport/ftp_impl.rs` | FTP operations shared by FTP and FTPS; gains `mod integration` at the end of the file | Modify (append test module, Tasks 1–7); doc comment on `check_ftp_path:187` (Task 8) |
| `src/transport/sftp.rs` | SFTP transport + existing SSH/SFTP harness | Modify at `:107` (host key callback) and `:1340` (test server), Task 9 |
| `Cargo.toml` | Dependency requirements | Modify, Tasks 8–12 |
| `Cargo.lock` | Resolved graph | Modify, Tasks 8–12 |

The harness lives in `ftp_impl.rs` rather than a new file because that is where the code under test lives, and because it mirrors the SFTP harness's placement at the bottom of `sftp.rs:1301`. `ftp_impl.rs` is 642 lines today; the harness will roughly double it, which stays within this codebase's norms (`sftp.rs` is 1951, `checkpoint.rs` 1852).

**Note on commit granularity:** the spec describes the harness as "commit 1". This plan splits it across Tasks 1–7, each with its own commit, because each adds independently testable behaviour. That is a refinement of the spec's sequence, not a departure from it.

---

### Task 1: FTP harness skeleton — connect, login, quit

**Files:**
- Modify: `src/transport/ftp_impl.rs` (append `mod integration` at end of file)

**Interfaces:**
- Consumes: `FtpTransport::connect` (`src/transport/ftp.rs`), `Session` (`crate::session`), `Transport` trait (`crate::transport`).
- Produces: `type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>`; `type Log = Arc<Mutex<Vec<String>>>`; `struct Faults` (all fields default `false`); `async fn start_server(store: Store, faults: Faults) -> (u16, Arc<AtomicUsize>, Log)`; `fn test_session(port: u16) -> Session`; `async fn handle_control(sock: TcpStream, store: Store, faults: Faults, log: Log) -> std::io::Result<()>`. Tasks 2–7 extend `handle_control`'s match arms and reuse all of these unchanged.

- [ ] **Step 1: Write the failing test**

Append to `src/transport/ftp_impl.rs`:

```rust
/// FTP integration tests against an in-process FTP server. No external
/// daemon is required. The server speaks only the commands blink issues and
/// serves data in short slices, so partial reads are exercised rather than
/// assumed away.
///
/// Written against suppaftp 8.0.5 deliberately: a harness written against a
/// new version cannot tell "encodes current behaviour" from "encodes the new
/// library's behaviour". Landed first, it is a differential test.
#[cfg(test)]
mod integration {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    use crate::session::{AuthMethod, Protocol, Session};
    use crate::transport::ftp::FtpTransport;
    use crate::transport::Transport;

    /// Absolute path -> file contents. A key ending in `/` is a directory.
    pub(super) type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    /// Every control-channel command the server received, verbatim and in
    /// order. Some behaviour is invisible from the result alone — a resume
    /// that silently restarts still lands the correct bytes — so tests assert
    /// on what was *issued*, not only on what came back.
    pub(super) type Log = Arc<Mutex<Vec<String>>>;

    /// Protocol-level malformations the server emits on demand. suppaftp 10.0
    /// converted these from panic to `FtpError`; no off-the-shelf server will
    /// produce them on request, which is why this one is hand-rolled.
    #[derive(Clone, Default)]
    pub(super) struct Faults {
        /// PASV reply carrying an out-of-range octet.
        pub bad_pasv_octet: bool,
        /// A LIST body no parser can turn into entries.
        pub unparsable_list_line: bool,
        /// Close control and data connections after `150`, sending no `226`.
        pub abrupt_close: bool,
    }

    fn test_session(port: u16) -> Session {
        Session {
            name: "it".to_string(),
            protocol: Protocol::Ftp,
            host: "127.0.0.1".to_string(),
            port,
            username: "tester".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        }
    }

    /// Binds :0, spawns the accept loop, returns the bound port and a count of
    /// accepted control connections. The dispatcher opens one per worker; the
    /// counter is how reuse is asserted, as in the SFTP harness.
    pub(super) async fn start_server(store: Store, faults: Faults) -> (u16, Arc<AtomicUsize>, Log) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connects = Arc::new(AtomicUsize::new(0));
        let connects_l = Arc::clone(&connects);
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let log_l = Arc::clone(&log);

        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                connects_l.fetch_add(1, Ordering::SeqCst);
                let store = Arc::clone(&store);
                let faults = faults.clone();
                let log = Arc::clone(&log_l);
                tokio::spawn(async move {
                    let _ = handle_control(sock, store, faults, log).await;
                });
            }
        });

        (port, connects, log)
    }

    /// One control connection. Extended by later tasks; unknown commands get
    /// `502` so a blink change that starts issuing a new command fails loudly
    /// instead of hanging.
    async fn handle_control(
        mut sock: TcpStream,
        _store: Store,
        _faults: Faults,
        log: Log,
    ) -> std::io::Result<()> {
        let (read_half, mut w) = sock.split();
        let mut lines = BufReader::new(read_half).lines();

        w.write_all(b"220 blink test server\r\n").await?;

        while let Some(line) = lines.next_line().await? {
            let line = line.trim_end();
            let (cmd, arg) = match line.split_once(' ') {
                Some((c, a)) => (c.to_ascii_uppercase(), a.to_string()),
                None => (line.to_ascii_uppercase(), String::new()),
            };
            // Every command verbatim, so a test can assert what was issued
            // rather than only what came back. Resume in particular is
            // invisible from the result alone: a download that silently
            // restarts still lands the correct bytes.
            log.lock().await.push(line.to_string());

            match cmd.as_str() {
                "USER" => w.write_all(b"331 password required\r\n").await?,
                "PASS" => w.write_all(b"230 logged in\r\n").await?,
                "TYPE" => w.write_all(b"200 type set\r\n").await?,
                "QUIT" => {
                    w.write_all(b"221 goodbye\r\n").await?;
                    break;
                }
                _ => w.write_all(b"502 command not implemented\r\n").await?,
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn connecting_logs_in_and_sets_binary_mode() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, connects, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw"))
            .await
            .expect("connect and login should succeed");

        assert_eq!(transport.protocol(), Protocol::Ftp);
        assert_eq!(connects.load(Ordering::SeqCst), 1);

        transport.close().await.expect("QUIT should be clean");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test integration::connecting_logs_in_and_sets_binary_mode`

Expected: a compile error. `FtpTransport` is likely not reachable as `crate::transport::ftp::FtpTransport` — check whether `mod ftp` is `pub(crate)` in `src/transport/mod.rs` and adjust the `use` to match. If the module is private, make it `pub(crate) mod ftp;`. This is the only production change in Task 1.

- [ ] **Step 3: Fix the import path so the test compiles and passes**

Adjust the `use` in the test module (and, only if required, the module visibility in `src/transport/mod.rs`) until the test builds.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test integration::connecting_logs_in_and_sets_binary_mode`
Expected: PASS.

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```
Expected: all tests pass, zero clippy warnings, `TOTAL over 1.90: 0`.

- [ ] **Step 6: Commit**

```bash
git add src/transport/ftp_impl.rs src/transport/mod.rs
git commit -m "test(ftp): add an in-process FTP server harness

FTP is the least-tested transport here: all six of its tests cover the
c8373c8 path guard and none reach a server. This adds the skeleton the
rest of the coverage hangs off — control connection, login, QUIT, and an
in-memory store, mirroring the SSH+SFTP harness in sftp.rs.

Unknown commands answer 502 rather than hanging, so a future change that
starts issuing a new command fails loudly."
```

---

### Task 2: PASV and LIST

**Files:**
- Modify: `src/transport/ftp_impl.rs` (`mod integration`)

**Interfaces:**
- Consumes: `handle_control`, `Store`, `Faults`, `start_server`, `test_session` from Task 1.
- Produces: `PASV` / `LIST` arms in `handle_control`; `fn listing_for(store: &HashMap<String, Vec<u8>>, dir: &str) -> String`. Tasks 3–7 rely on `PASV` leaving a bound `TcpListener` in the `pasv` local for the next data command to consume.

- [ ] **Step 1: Write the failing test**

Add inside `mod integration`:

```rust
    #[tokio::test]
    async fn listing_a_directory_returns_its_entries() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/pub/one.txt".to_string(), b"hello".to_vec());
            files.insert("/pub/two.bin".to_string(), vec![0u8; 4096]);
            files.insert("/pub/sub/".to_string(), Vec::new());
            // Not an immediate child; must not appear.
            files.insert("/pub/sub/deep.txt".to_string(), b"x".to_vec());
        }
        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let mut entries = transport.list("/pub").await.expect("list should succeed");
        entries.sort_by(|a, b| a.raw_name.cmp(&b.raw_name));

        let names: Vec<&str> = entries.iter().map(|e| e.raw_name.as_str()).collect();
        assert_eq!(names, vec!["one.txt", "sub", "two.bin"]);

        let one = &entries[0];
        assert_eq!(one.size, 5);
        assert_eq!(one.kind, crate::transport::EntryKind::File);

        let sub = &entries[1];
        assert_eq!(sub.kind, crate::transport::EntryKind::Directory);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test integration::listing_a_directory_returns_its_entries`
Expected: FAIL — the server answers `LIST` with `502`, so `list` returns an error.

- [ ] **Step 3: Implement PASV and LIST**

Add above `handle_control`:

```rust
    /// Unix `ls -l` style listing of the immediate children of `dir`.
    /// suppaftp's parser expects this shape; a key ending in `/` is a
    /// directory and renders with a `d` mode prefix.
    fn listing_for(files: &HashMap<String, Vec<u8>>, dir: &str) -> String {
        let prefix = if dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}/")
        };

        let mut out = String::new();
        for (path, bytes) in files {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let trimmed = rest.trim_end_matches('/');
            // Immediate children only: no interior separator.
            if trimmed.is_empty() || trimmed.contains('/') {
                continue;
            }
            let (mode, size) = if path.ends_with('/') {
                ("drwxr-xr-x", 4096)
            } else {
                ("-rw-r--r--", bytes.len())
            };
            out.push_str(&format!(
                "{mode} 1 owner group {size:>12} Nov 01 12:00 {trimmed}\r\n"
            ));
        }
        out
    }
```

Change `handle_control`'s signature to use `store` and `faults` (drop the leading underscores), and add these locals before the loop:

```rust
        // Bound by PASV, consumed by the next data command.
        let mut pasv: Option<TcpListener> = None;
```

Add the match arms:

```rust
                "PASV" => {
                    let data_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
                    let port = data_listener.local_addr()?.port();
                    pasv = Some(data_listener);
                    let reply = if faults.bad_pasv_octet {
                        "227 Entering Passive Mode (127,0,0,1,999,0)\r\n".to_string()
                    } else {
                        format!(
                            "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                            port / 256,
                            port % 256
                        )
                    };
                    w.write_all(reply.as_bytes()).await?;
                }
                "LIST" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    let body = if faults.unparsable_list_line {
                        "!! this is not a listing line !!\r\n".to_string()
                    } else {
                        let files = store.lock().await;
                        let dir = if arg.is_empty() { "/" } else { arg.as_str() };
                        listing_for(&files, dir)
                    };
                    w.write_all(b"150 here comes the listing\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    data.write_all(body.as_bytes()).await?;
                    data.shutdown().await?;
                    drop(data);
                    w.write_all(b"226 transfer complete\r\n").await?;
                }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test integration::listing_a_directory_returns_its_entries`
Expected: PASS.

If entry kinds come back wrong, check the mode prefix — suppaftp keys the directory flag off the leading `d`. If sizes are wrong, check the column alignment in `listing_for`; the parser is whitespace-tolerant but expects the size in the fifth field.

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```

- [ ] **Step 6: Commit**

```bash
git add src/transport/ftp_impl.rs
git commit -m "test(ftp): cover PASV and LIST parsing

The harness now serves a Unix ls -l listing over a passive data
connection, so ftp_list's parse into RemoteEntry is exercised — names,
sizes, and the file/directory split — against a real socket rather than a
fixture string."
```

---

### Task 3: SIZE, RETR, and download

**Files:**
- Modify: `src/transport/ftp_impl.rs` (`mod integration`)

**Interfaces:**
- Consumes: `pasv` local and everything from Tasks 1–2.
- Produces: `SIZE` / `RETR` arms; `const DATA_SLICE: usize = 4096`; `rest` local (set by Task 5's `REST`, read by `RETR`).

- [ ] **Step 1: Write the failing test**

```rust
    /// Deterministic pseudo-random bytes (xorshift64), matching the SFTP
    /// harness's helper so payloads are reproducible across runs.
    fn pseudo_random(n: usize) -> Vec<u8> {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(n);
        out
    }

    #[tokio::test]
    async fn downloading_preserves_every_byte() {
        let payload = pseudo_random(200_000);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/big.bin".to_string(), payload.clone());

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let dir = tempdir_for_test();
        let local = dir.join("big.bin");
        transport
            .download("/big.bin", &local, None)
            .await
            .expect("download should succeed");

        let got = std::fs::read(&local).unwrap();
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn metadata_reports_the_size_the_server_gave() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/a.txt".to_string(), b"twelve bytes".to_vec());

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let meta = transport.metadata("/a.txt").await.unwrap().expect("present");
        assert_eq!(meta.size, 12);
    }
```

Add the temp-dir helper (the codebase already isolates config dirs in tests; follow whatever `src/paths.rs` exposes if a helper exists, otherwise):

```rust
    /// A unique scratch directory for one test, removed by the OS on reboot.
    /// Tests that write files use this rather than the repo tree.
    fn tempdir_for_test() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "blink-ftp-it-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test integration::downloading_preserves_every_byte integration::metadata_reports_the_size_the_server_gave`
Expected: FAIL — `SIZE` and `RETR` answer `502`.

- [ ] **Step 3: Implement SIZE and RETR**

Add near the top of `mod integration`:

```rust
    /// Bytes per data-connection write. Smaller than the transfer chunk so the
    /// client's read loop genuinely reassembles across reads.
    const DATA_SLICE: usize = 4096;
```

Add the `rest` local beside `pasv`:

```rust
        // Set by REST, consumed by the next RETR, then cleared.
        let mut rest: u64 = 0;
```

Add the arms:

```rust
                "SIZE" => {
                    let files = store.lock().await;
                    match files.get(&arg) {
                        Some(bytes) => {
                            w.write_all(format!("213 {}\r\n", bytes.len()).as_bytes())
                                .await?
                        }
                        None => w.write_all(b"550 no such file\r\n").await?,
                    }
                }
                "RETR" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    let body = {
                        let files = store.lock().await;
                        match files.get(&arg) {
                            Some(bytes) => bytes.clone(),
                            None => {
                                w.write_all(b"550 no such file\r\n").await?;
                                continue;
                            }
                        }
                    };
                    let start = std::mem::take(&mut rest) as usize;
                    let slice = body.get(start..).unwrap_or(&[]).to_vec();

                    w.write_all(b"150 opening data connection\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    for chunk in slice.chunks(DATA_SLICE) {
                        data.write_all(chunk).await?;
                    }
                    data.shutdown().await?;
                    drop(data);
                    w.write_all(b"226 transfer complete\r\n").await?;
                }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test integration::downloading_preserves_every_byte integration::metadata_reports_the_size_the_server_gave`
Expected: PASS both.

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```

- [ ] **Step 6: Commit**

```bash
git add src/transport/ftp_impl.rs
git commit -m "test(ftp): cover SIZE and download round-trips

Payloads are served in 4 KiB slices so the client reassembles across many
reads rather than getting one convenient buffer, and the download test
uses 200 KB — larger than a transfer chunk — so the loop is real."
```

---

### Task 4: STOR and upload

**Files:**
- Modify: `src/transport/ftp_impl.rs` (`mod integration`)

**Interfaces:**
- Consumes: everything from Tasks 1–3.
- Produces: `STOR` arm (Task 5 extends the same arm to handle `APPE`).

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn uploading_preserves_every_byte() {
        let payload = pseudo_random(150_000);
        let dir = tempdir_for_test();
        let local = dir.join("up.bin");
        std::fs::write(&local, &payload).unwrap();

        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport
            .upload(&local, "/up.bin", None)
            .await
            .expect("upload should succeed");

        let files = store.lock().await;
        assert_eq!(files.get("/up.bin").map(Vec::as_slice), Some(&payload[..]));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test integration::uploading_preserves_every_byte`
Expected: FAIL — `STOR` answers `502`.

- [ ] **Step 3: Implement STOR**

Add `AsyncReadExt` to the `tokio::io` import, then add the arm:

```rust
                "STOR" => {
                    let Some(data_listener) = pasv.take() else {
                        w.write_all(b"425 use PASV first\r\n").await?;
                        continue;
                    };
                    w.write_all(b"150 ready for data\r\n").await?;
                    let (mut data, _) = data_listener.accept().await?;
                    if faults.abrupt_close {
                        return Ok(());
                    }
                    let mut buf = Vec::new();
                    data.read_to_end(&mut buf).await?;
                    drop(data);
                    store.lock().await.insert(arg.clone(), buf);
                    w.write_all(b"226 transfer complete\r\n").await?;
                }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test integration::uploading_preserves_every_byte`
Expected: PASS.

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```

- [ ] **Step 6: Commit**

```bash
git add src/transport/ftp_impl.rs
git commit -m "test(ftp): cover STOR upload round-trips"
```

---

### Task 5: REST and APPE — the resume paths

**Files:**
- Modify: `src/transport/ftp_impl.rs` (`mod integration`)

**Interfaces:**
- Consumes: `rest` local (Task 3), `STOR` arm (Task 4).
- Produces: `REST` arm; `APPE` folded into the `STOR` arm.

These are the highest-value tests in the harness: resume is the FTP behaviour most likely to shift under a major bump, and nothing covers it today.

- [ ] **Step 1: Write the failing tests**

```rust
    /// A `.part` file from an interrupted download must continue from its
    /// length via REST, not restart and not concatenate.
    #[tokio::test]
    async fn a_partial_download_resumes_from_its_offset() {
        let payload = pseudo_random(100_000);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/resume.bin".to_string(), payload.clone());

        let dir = tempdir_for_test();
        let local = dir.join("resume.bin");

        // Seed the partial exactly as an interrupted download leaves it —
        // BOTH the bytes and the provenance sidecar. `decide_resume` refuses
        // to continue a partial it cannot identify, so without the sidecar
        // this test would silently exercise a fresh download instead, and
        // still pass: restarting from zero also lands the correct bytes.
        // That is why the REST assertion below is the real assertion.
        std::fs::write(
            crate::transport::part_path(&local),
            &payload[..30_000],
        )
        .unwrap();
        crate::transport::write_part_meta(&local, "/resume.bin", Some(payload.len() as u64))
            .await;

        let (port, _c, log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport.download("/resume.bin", &local, None).await.unwrap();

        let got = std::fs::read(&local).unwrap();
        assert_eq!(got.len(), payload.len(), "resumed file must be whole");
        assert_eq!(got, payload, "resumed bytes must match, not duplicate");

        let issued = log.lock().await.clone();
        assert!(
            issued.iter().any(|c| c == "REST 30000"),
            "the download must resume from the partial's length, not restart; \
             commands issued: {issued:?}",
        );
    }

    /// APPE must extend the remote file, not truncate it.
    #[tokio::test]
    async fn appending_extends_rather_than_truncating() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store
            .lock()
            .await
            .insert("/app.bin".to_string(), b"first-".to_vec());

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;

        // Drive APPE directly: the client-side resume policy is not what is
        // under test here, the server contract is.
        let mut sock = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (r, mut w) = sock.split();
        let mut lines = BufReader::new(r).lines();
        lines.next_line().await.unwrap(); // 220

        w.write_all(b"PASV\r\n").await.unwrap();
        let pasv_line = lines.next_line().await.unwrap().unwrap();
        let data_port = parse_pasv_port(&pasv_line);

        w.write_all(b"APPE /app.bin\r\n").await.unwrap();
        lines.next_line().await.unwrap(); // 150
        let mut data = TcpStream::connect(("127.0.0.1", data_port)).await.unwrap();
        data.write_all(b"second").await.unwrap();
        data.shutdown().await.unwrap();
        drop(data);
        lines.next_line().await.unwrap(); // 226

        let files = store.lock().await;
        assert_eq!(files.get("/app.bin").unwrap().as_slice(), b"first-second");
    }

    /// `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)` -> port.
    fn parse_pasv_port(line: &str) -> u16 {
        let inner = line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inner, _)| inner)
            .expect("PASV reply should carry a tuple");
        let parts: Vec<u16> = inner.split(',').map(|p| p.trim().parse().unwrap()).collect();
        parts[4] * 256 + parts[5]
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test integration::a_partial_download_resumes_from_its_offset integration::appending_extends_rather_than_truncating`
Expected: FAIL — `REST` and `APPE` answer `502`.

- [ ] **Step 3: Implement REST and APPE**

Add the `REST` arm:

```rust
                "REST" => {
                    rest = arg.trim().parse().unwrap_or(0);
                    w.write_all(b"350 restart position accepted\r\n").await?;
                }
```

Change the `STOR` arm's pattern to `"STOR" | "APPE"` and replace its store write with:

```rust
                    {
                        let mut files = store.lock().await;
                        if cmd == "APPE" {
                            files.entry(arg.clone()).or_default().extend_from_slice(&buf);
                        } else {
                            files.insert(arg.clone(), buf);
                        }
                    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test integration::a_partial_download_resumes_from_its_offset integration::appending_extends_rather_than_truncating`
Expected: PASS both.

If the resume test fails with a doubled-length file, the client is not sending `REST` and is restarting from zero — that is a real finding about `ftp_download`, not a harness bug. Stop and report it rather than adjusting the test to match.

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```

- [ ] **Step 6: Commit**

```bash
git add src/transport/ftp_impl.rs
git commit -m "test(ftp): cover REST and APPE resume

Resume is the FTP behaviour most likely to shift under a major version
bump and had no coverage at all. The download test pre-seeds a .part file
the way an interrupted transfer leaves one and asserts the result is whole
and not duplicated."
```

---

### Task 6: Mutating commands — rename, mkdir, rmdir, delete

**Files:**
- Modify: `src/transport/ftp_impl.rs` (`mod integration`)

**Interfaces:**
- Consumes: everything from Tasks 1–5.
- Produces: `RNFR` / `RNTO` / `MKD` / `RMD` / `DELE` arms; `rename_from` local.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn rename_mkdir_rmdir_and_delete_reach_the_server() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut files = store.lock().await;
            files.insert("/old.txt".to_string(), b"body".to_vec());
            files.insert("/doomed.txt".to_string(), b"x".to_vec());
            files.insert("/emptydir/".to_string(), Vec::new());
        }

        let (port, _c, _log) = start_server(Arc::clone(&store), Faults::default()).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        transport.rename("/old.txt", "/new.txt").await.unwrap();
        transport.mkdir("/fresh").await.unwrap();
        transport.delete_file("/doomed.txt").await.unwrap();
        transport.delete_dir("/emptydir", false).await.unwrap();

        let files = store.lock().await;
        assert!(files.contains_key("/new.txt"), "rename should move the key");
        assert!(!files.contains_key("/old.txt"), "old name should be gone");
        assert_eq!(files.get("/new.txt").unwrap().as_slice(), b"body");
        assert!(files.contains_key("/fresh/"), "mkdir should create a dir key");
        assert!(!files.contains_key("/doomed.txt"), "delete should remove");
        assert!(!files.contains_key("/emptydir/"), "rmdir should remove");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test integration::rename_mkdir_rmdir_and_delete_reach_the_server`
Expected: FAIL — all five commands answer `502`.

- [ ] **Step 3: Implement the mutating commands**

Add the local beside `pasv` and `rest`:

```rust
        // Set by RNFR, consumed by RNTO.
        let mut rename_from = String::new();
```

Add the arms:

```rust
                "RNFR" => {
                    rename_from = arg.clone();
                    w.write_all(b"350 ready for RNTO\r\n").await?;
                }
                "RNTO" => {
                    let mut files = store.lock().await;
                    match files.remove(&rename_from) {
                        Some(bytes) => {
                            files.insert(arg.clone(), bytes);
                            drop(files);
                            w.write_all(b"250 renamed\r\n").await?;
                        }
                        None => {
                            drop(files);
                            w.write_all(b"550 no such file\r\n").await?;
                        }
                    }
                }
                "MKD" => {
                    let key = format!("{}/", arg.trim_end_matches('/'));
                    store.lock().await.insert(key, Vec::new());
                    w.write_all(b"257 directory created\r\n").await?;
                }
                "RMD" => {
                    let key = format!("{}/", arg.trim_end_matches('/'));
                    let removed = store.lock().await.remove(&key).is_some();
                    if removed {
                        w.write_all(b"250 directory removed\r\n").await?;
                    } else {
                        w.write_all(b"550 no such directory\r\n").await?;
                    }
                }
                "DELE" => {
                    let removed = store.lock().await.remove(&arg).is_some();
                    if removed {
                        w.write_all(b"250 file deleted\r\n").await?;
                    } else {
                        w.write_all(b"550 no such file\r\n").await?;
                    }
                }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test integration::rename_mkdir_rmdir_and_delete_reach_the_server`
Expected: PASS.

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```

- [ ] **Step 6: Commit**

```bash
git add src/transport/ftp_impl.rs
git commit -m "test(ftp): cover rename, mkdir, rmdir and delete"
```

---

### Task 7: Fault injection

**Files:**
- Modify: `src/transport/ftp_impl.rs` (`mod integration`)

**Interfaces:**
- Consumes: `Faults` (Task 1), and the `bad_pasv_octet` / `unparsable_list_line` / `abrupt_close` branches already wired into Tasks 2–4.
- Produces: three tests. No new server code — the branches exist; this task proves they behave.

This is the task that justifies hand-rolling the server. suppaftp 10.0 converted exactly these cases from `panic!` to `FtpError`, and Task 8 needs a before/after reading on them.

- [ ] **Step 1: Write the failing tests**

```rust
    /// An out-of-range PASV octet must surface as an error, not a panic and
    /// not a hang. suppaftp 10.0 changed this from a panic; these tests pin
    /// the behaviour on both sides of that bump.
    #[tokio::test]
    async fn a_malformed_pasv_reply_is_an_error_not_a_panic() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store.lock().await.insert("/a.txt".to_string(), b"x".to_vec());

        let faults = Faults {
            bad_pasv_octet: true,
            ..Faults::default()
        };
        let (port, _c, _log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let result = transport.list("/").await;
        assert!(result.is_err(), "a 999 octet must not be accepted");
    }

    #[tokio::test]
    async fn an_unparsable_listing_line_is_an_error_not_a_panic() {
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store.lock().await.insert("/a.txt".to_string(), b"x".to_vec());

        let faults = Faults {
            unparsable_list_line: true,
            ..Faults::default()
        };
        let (port, _c, _log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        // Either an error or an empty listing is acceptable — a panic is not,
        // and neither is a garbage entry addressed at a nonexistent path.
        match transport.list("/").await {
            Err(_) => {}
            Ok(entries) => assert!(
                entries.is_empty(),
                "an unparsable line must not become an entry: {entries:?}"
            ),
        }
    }

    #[tokio::test]
    async fn a_connection_dropped_mid_transfer_is_reported_not_hung() {
        let payload = pseudo_random(50_000);
        let store: Store = Arc::new(Mutex::new(HashMap::new()));
        store.lock().await.insert("/gone.bin".to_string(), payload);

        let faults = Faults {
            abrupt_close: true,
            ..Faults::default()
        };
        let (port, _c, _log) = start_server(Arc::clone(&store), faults).await;
        let session = test_session(port);
        let mut transport = FtpTransport::connect(&session, Some("pw")).await.unwrap();

        let dir = tempdir_for_test();
        let local = dir.join("gone.bin");

        // Must not hang: FTP_OP_TIMEOUT is 60s, so a 10s bound proves the
        // failure comes from the closed socket rather than the deadline.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            transport.download("/gone.bin", &local, None),
        )
        .await;

        let inner = result.expect("must fail fast, not wait out the timeout");
        assert!(inner.is_err(), "a dropped connection must be an error");
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test integration::a_malformed_pasv_reply integration::an_unparsable_listing_line integration::a_connection_dropped_mid_transfer`

Expected on suppaftp 8.0.5: they may **panic** rather than fail cleanly — that is the documented pre-10.0 behaviour and is the finding this task exists to record.

- [ ] **Step 3: Record the observed behaviour**

If a test panics on 8.0.5, mark it `#[ignore = "suppaftp 8.0.5 panics here; unignored by the 10.0 bump in the next task"]` with that exact reason, and note which ones in the commit message. Do **not** weaken the assertion to make it pass — the whole point is that Task 8 flips these.

If they already pass on 8.0.5, say so in the commit message; that is equally useful information and means the bump is a smaller behavioural change than the changelog implies.

- [ ] **Step 4: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && python3 /tmp/msrv.py
```

- [ ] **Step 5: Commit**

```bash
git add src/transport/ftp_impl.rs
git commit -m "test(ftp): inject malformed protocol responses

A bad PASV octet, an unparsable LIST line, and a connection dropped
mid-transfer. suppaftp 10.0 converts these from panic to FtpError, so
pinning them here — against 8.0.5 — is what makes the next commit a
measurement rather than a hope.

[State which tests pass and which are #[ignore]d as panicking.]"
```

---

### Task 8: suppaftp 8 → 10

**Files:**
- Modify: `Cargo.toml:31`, `Cargo.lock`
- Modify: `src/transport/ftp_impl.rs:175-197` (doc comment on `check_ftp_path` only)

**Interfaces:**
- Consumes: the whole harness from Tasks 1–7.
- Produces: no API change. `check_ftp_path`'s signature and behaviour are unchanged.

- [ ] **Step 1: Bump the requirement**

In `Cargo.toml`, change:

```toml
suppaftp = { version = "8.0", features = ["tokio", "rustls-ring", "tokio-rustls-ring"] }
```

to:

```toml
suppaftp = { version = "10.0", features = ["tokio", "rustls-ring", "tokio-rustls-ring"] }
```

Then: `cargo check --all-targets`
Expected: compiles with zero errors. (Verified during planning — suppaftp 9.0's breaking changes were the async-std→smol migration, which does not touch a tokio consumer, and 10.0's `tcp_stream()` signature change affects a method blink never calls.)

- [ ] **Step 2: Un-ignore any fault tests Task 7 marked**

Remove the `#[ignore]` attributes added in Task 7 Step 3, if any.

- [ ] **Step 3: Run the tests**

Run: `cargo test`
Expected: everything passes, including the previously-ignored fault tests. Malformed responses now produce a `BlinkError` instead of a panic.

If a *non-fault* test changes behaviour, stop and report it — that is a real regression in the bump and the plan's assumption that this is a no-code-change upgrade is wrong.

- [ ] **Step 4: Record the guard reconciliation**

In `src/transport/ftp_impl.rs`, append to the doc comment on `check_ftp_path` (immediately before `pub(crate) fn check_ftp_path` at line 187):

```rust
/// suppaftp 10.0.2 also rejects CR/LF at the library boundary, so this is no
/// longer the only guard. It stays the primary one: it names the operation
/// and produces a sanitized `BlinkError::transport`, where suppaftp's would
/// arrive as an opaque `FtpError`. Do not remove it as redundant.
```

- [ ] **Step 5: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && cargo audit --deny warnings && python3 /tmp/msrv.py
```

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/transport/ftp_impl.rs
git commit -m "chore(deps): suppaftp 8 -> 10

No source changes. 9.0's breaking changes were the async-std to smol
migration, which does not reach a tokio consumer; 10.0 changed tcp_stream()
and DataStream::into_tcp_stream to return FtpResult, neither of which blink
calls.

What the bump does buy is error handling: 10.0 converts unwrap/expect/panic
paths to FtpError, so a malformed PASV octet or an unparsable LIST line is
now a clean failure. The harness added in the preceding commits measures
that rather than assuming it.

10.0.2 also rejects CR/LF at the library boundary. blink's check_ftp_path
stays the primary guard — it names the operation and sanitizes — and its
doc comment now records that upstream backstops it."
```

---

### Task 9: russh 0.60 → 0.63

**Files:**
- Modify: `Cargo.toml:27`, `Cargo.lock`
- Modify: `src/transport/sftp.rs:104-112` (the `check_server_key` signature and its new match)
- Modify: `src/transport/sftp.rs:1338-1343` (test server `channel_open_session`)

**Interfaces:**
- Consumes: `known_hosts::check`, `KeyStatus`, `AppEvent`, `SessionTrust` — all unchanged.
- Produces: `check_server_key` now takes `&russh::keys::PublicKeyOrCertificate`. The `PublicKey` arm's behaviour is bit-for-bit what it was; the `Certificate` arm is new and always rejects.

- [ ] **Step 1: Bump and see the two errors**

In `Cargo.toml`, change `russh = "0.60"` to `russh = "0.63"`, then:

Run: `cargo check --all-targets`

Expected: exactly two errors —
- `E0053` at `src/transport/sftp.rs:107`: `check_server_key` expected `&PublicKeyOrCertificate`, found `&russh::keys::PublicKey`.
- `E0050` at `src/transport/sftp.rs:1340`: `channel_open_session` has 3 parameters but the trait declares 4.

- [ ] **Step 2: Write the failing test for the certificate arm**

Add to `mod integration` in `src/transport/sftp.rs`:

```rust
    /// Build a self-signed SSH *host* certificate. Nothing validates it — the
    /// point is only that the callback receives the `Certificate` variant.
    /// `Builder::sign` refuses an empty principal list ("golden ticket"), so
    /// `all_principals_valid` is set explicitly.
    fn host_certificate() -> russh::keys::ssh_key::Certificate {
        use russh::keys::ssh_key::certificate::{Builder, CertType};

        let ca = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let subject = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();

        let mut builder = Builder::new(
            [0u8; 16],
            subject.public_key().key_data().clone(),
            0,
            u64::MAX >> 1,
        )
        .unwrap();
        builder.cert_type(CertType::Host).unwrap();
        builder.all_principals_valid().unwrap();
        builder.key_id("blink-test").unwrap();
        builder.sign(&ca).unwrap()
    }

    /// A server presenting an SSH host *certificate* must be refused. blink's
    /// known_hosts stores host -> (key type, base64 key) and has no
    /// @cert-authority support, so it cannot validate a certificate's CA
    /// signature, principals, or validity window. TOFU-pinning one would look
    /// like verification while checking none of that; russh 0.60 could not
    /// surface a certificate here at all, so refusing preserves behaviour.
    #[tokio::test]
    async fn a_host_certificate_is_refused() {
        use russh::client::Handler as _;
        use russh::keys::PublicKeyOrCertificate;

        let (ev_tx, mut ev_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::tui::event::AppEvent>();
        let mut handler = super::KnownHostsHandler {
            host: "example.test".to_string(),
            port: 22,
            event_tx: Some(ev_tx),
            trust: crate::known_hosts::SessionTrust::new(),
        };

        let accepted = handler
            .check_server_key(&PublicKeyOrCertificate::Certificate(host_certificate()))
            .await
            .expect("the callback itself must not error");

        assert!(!accepted, "a host certificate must be refused");
        assert!(
            ev_rx.try_recv().is_err(),
            "a certificate must not raise the trust-on-first-use prompt",
        );
    }
```

`KnownHostsHandler` is declared at `src/transport/sftp.rs:73` with exactly the four fields used above — `host`, `port`, `event_tx`, `trust` — matching the real construction site at `:346`. It is private to the module, so this test must live in a child module of `sftp.rs` (`mod integration` qualifies, via `super::`).

`rand` is already a `[dev-dependencies]` entry and `rand::rng()` is already used by the SFTP harness for key generation, so no new dependency is needed.

- [ ] **Step 3: Fix the host key callback**

In `src/transport/sftp.rs`, replace the signature at line 105-108:

```rust
    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
```

with:

```rust
    async fn check_server_key(
        &mut self,
        presented: &russh::keys::PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // blink has no @cert-authority support: known_hosts stores a host
        // against a literal key, so there is nothing to validate a
        // certificate's CA signature, principals, or validity window
        // against. Pinning one by its key would look like verification
        // while checking none of that, so refuse — fail closed, as the
        // known-hosts read error below does. russh 0.60 could not surface
        // a certificate to this callback, so nothing that works today
        // starts failing.
        let server_public_key = match presented {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                tracing::warn!(
                    host = %self.display_host(),
                    "server presented a host certificate — rejecting: \
                     blink does not support host certificates",
                );
                return Ok(false);
            }
        };
```

The rest of the function body is unchanged — `server_public_key` is now a `&PublicKey` binding rather than a parameter, so every use below it still compiles.

- [ ] **Step 4: Fix the test server's channel_open_session**

At `src/transport/sftp.rs:1338`, add the fourth parameter:

```rust
        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            _handle: russh::server::ChannelOpenHandleInner<Msg>,
            _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
```

Check the exact parameter position and type against the compiler's `note:` line from Step 1 — it prints the full trait signature.

- [ ] **Step 5: Run the tests**

Run: `cargo test`
Expected: all pass, including the 14 existing SFTP integration tests and the new certificate test.

- [ ] **Step 6: Run the full gate**

```bash
cargo test && cargo clippy --all-targets && cargo audit --deny warnings && python3 /tmp/msrv.py
```

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/transport/sftp.rs
git commit -m "chore(deps): russh 0.60 -> 0.63, refusing host certificates

0.63 widens check_server_key from &PublicKey to &PublicKeyOrCertificate.
The PublicKey arm keeps the existing body verbatim; the Certificate arm is
new and refuses.

Refusing is the conservative reading. known_hosts stores a host against a
literal key and blink has no @cert-authority support, so there is nothing
to check a certificate's CA signature, principals, or validity window
against. TOFU-pinning it by key would look like verification while
checking none of that. russh 0.60 could not surface a certificate to this
callback, so no connection that works today begins to fail.

The other change is the in-process test server's channel_open_session,
which gains a ChannelOpenHandleInner parameter."
```

---

### Task 10: sha2 0.10 → 0.11

**Files:**
- Modify: `Cargo.toml:57`, `Cargo.lock`

- [ ] **Step 1: Bump**

Change `sha2 = { version = "0.10", default-features = false }` to `sha2 = { version = "0.11", default-features = false }`.

- [ ] **Step 2: Run the gate**

```bash
cargo test && cargo clippy --all-targets && cargo audit --deny warnings && python3 /tmp/msrv.py
```
Expected: zero compile errors, all tests pass. The `Digest` / `Sha256::new` / `update` / `finalize` path blink uses in `checkpoint.rs:269`, `session.rs:173`, and `ftps.rs:122` is unchanged across the major.

- [ ] **Step 3: Confirm no new duplicate**

Run: `grep -A1 'name = "sha2"' Cargo.lock | grep version`
Expected: `0.10.9` and `0.11.0`, the same two as before. russh reaches 0.10.9 through `bcrypt-pbkdf` and `ssh-encoding`, and 0.11.0 through `ed25519-dalek`, `p256`, and `p384`; this moves blink's own hashing onto the copy already compiled rather than adding one.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): sha2 0.10 -> 0.11

No source changes: the Digest/new/update/finalize path is unchanged across
the major. This adds no duplicate either — the tree already carries both
0.10.9 and 0.11.0 because russh reaches each through a different
dependency, so blink's own hashing simply moves onto the copy that is
already being compiled."
```

---

### Task 11: base64 0.22 → 0.23

**Files:**
- Modify: `Cargo.toml:53`, `Cargo.lock`

- [ ] **Step 1: Bump**

Change `base64 = "0.22"` to `base64 = "0.23"`.

- [ ] **Step 2: Run the gate**

```bash
cargo test && cargo clippy --all-targets && cargo audit --deny warnings && python3 /tmp/msrv.py
```
Expected: zero errors. The `Engine` trait plus `general_purpose::STANDARD` used in `preview.rs:11-12` and `sftp.rs:113-115` is unchanged.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): base64 0.22 -> 0.23

No source changes; the Engine + general_purpose::STANDARD path is
unchanged."
```

---

### Task 12: icy_sixel 0.5 → 0.6

**Files:**
- Modify: `Cargo.toml:55`, `Cargo.lock`

- [ ] **Step 1: Bump**

Change `icy_sixel = "0.5"` to `icy_sixel = "0.6"`.

- [ ] **Step 2: Run the gate**

```bash
cargo test && cargo clippy --all-targets && cargo audit --deny warnings && python3 /tmp/msrv.py
```
Expected: zero errors. `SixelImage::from_rgba` at `preview.rs:365` is unchanged. The MSRV walk must still read `TOTAL over 1.90: 0` — icy_sixel 0.6 pulls quantette 0.6, which declares 1.90 exactly.

- [ ] **Step 3: Sanity-check the sixel path by eye**

The suite does not render sixel. Run the app against any session, open an image file in the preview pane on a sixel-capable terminal, and confirm it still draws. If no sixel terminal is available, say so in the commit message rather than implying it was verified.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(deps): icy_sixel 0.5 -> 0.6

No source changes; SixelImage::from_rgba is unchanged. quantette moves to
0.6 with it, which still declares rust-version 1.90 and so stays within
the floor.

[State whether the sixel preview was confirmed by eye or not.]"
```

---

## Final verification

After Task 12:

- [ ] `cargo test` — all pass; the count should be roughly 410 + 11 new FTP tests + 1 certificate test.
- [ ] `cargo clippy --all-targets` — zero warnings.
- [ ] `cargo audit --deny warnings` — exit zero.
- [ ] `python3 /tmp/msrv.py` — `TOTAL over 1.90: 0`.
- [ ] `cargo build --release` — succeeds; `./target/release/blink --version` runs.
- [ ] `git log --oneline` — read the series back and confirm each commit stands alone.
- [ ] Report how many commits ahead of upstream `main` is. **Do not push.**

## Known gaps this plan does not close

- **FTPS keeps only its three unit tests.** Wrapping the harness in rustls is a materially larger job and is deliberately out of scope; the `AUTH TLS` upgrade and certificate pinning stay unexercised end to end.
- **The harness is a well-behaved server.** It does not reproduce vsftpd's or IIS's dialects, NAT-mangled PASV addresses, or TLS session reuse. A manual smoke test against a real server before release is still worth doing.
- **The MSRV gate is metadata, not compilation.** A genuine guarantee needs `rustup toolchain install 1.90 && cargo +1.90 check`.
- **The harness code in Tasks 1–7 was written from the API, not compiled.** The five upgrades were each compiled against the tree during planning, and every type the harness touches was read from source, but the server itself has not been run. Expect small fixes — a borrow in the `split()` halves, an `await` on a lock, a listing column the parser reads differently. The tests are the specification; adjust the server to satisfy them, not the reverse.
