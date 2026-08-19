# blink

A cross-platform terminal SFTP / SCP / FTP / FTPS client with a three-pane TUI, built using Claude.

![rust](https://img.shields.io/badge/rust-2024%20edition-orange)
![license](https://img.shields.io/badge/license-MIT-blue)

![blink](images/main-menu.jpg)

## Features

### Connectivity

- **SFTP** with password, SSH key, and encrypted-key (passphrase prompt) auth
- **SCP** as transparent SFTP — matches OpenSSH 9.0+ behavior, full feature parity
- **FTP** with anonymous and password auth
- **FTPS** with explicit TLS (RFC 4217) via rustls — pure Rust, no system
  TLS library needed
- **ssh-agent** auth on Unix (uses `$SSH_AUTH_SOCK`); on Windows, the
  built-in OpenSSH agent (`\\.\pipe\openssh-ssh-agent`) is tried first,
  falling back to Pageant
- **Host-key verification** for SFTP/SCP — unknown keys trigger an
  interactive prompt (accept & save / trust once / reject); changed keys are
  hard-rejected with a clear warning. Keys are stored in
  `~/.config/blink/known_hosts` in standard OpenSSH format.
- One-key disconnect, return to selector

### Browsing & file operations

- Three-pane TUI (local / remote, plus a switchable transfers/log panel)
- Recursive **download** and **upload** with parallel slot dispatch
- **Rename**, **create directories**, **delete files**, **delete directories** (recursive)
- **Substring filter** per pane (`/`), persists across refresh — selections
  made while filtered survive clearing or narrowing the filter too; the
  footer's selection count and size cover the whole selection, including
  entries the filter is currently hiding
- **Refresh** active pane (F5) — refreshing in place no longer blanks the
  pane while the new listing is fetched
- **Disconnect** and return to the session selector (`Ctrl-X`)
- **View** text files inline (scrollable, line-numbered, syntax-highlighted) —
  control and ANSI escape characters are stripped before display to prevent
  terminal injection; see [Supported viewer formats](#supported-viewer-formats)
  for the recognised extensions
- **View** images via [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/),
  [sixel](https://en.wikipedia.org/wiki/Sixel), and iTerm2 inline images —
  auto-detected, aspect-preserving, terminal-cell-aware scaling

### Transfers

- Parallel slot dispatcher (configurable globally; per-session override)
- **Pipelined SFTP streams** — each transfer keeps multiple read/write
  requests in flight at once instead of waiting a full round-trip per
  chunk, so a single stream is bandwidth-bound rather than latency-bound
  (the way OpenSSH's own `sftp` client works). This stacks on top of the
  parallel slot dispatcher for high aggregate throughput
- Live transfer strip with bytes, percentage, MB/s
- **Pause / resume** all transfers (`p`)
- **Cancel** individual in-flight transfers (`c`)
- **Cancel whole batch** for recursive transfers (`C`) — aborts every
  active and queued job from the same `Ctrl-D` / `Ctrl-U`
- **Overwrite confirmation** with three choices — overwrite all / skip
  conflicts / cancel — for both downloads and uploads, single-file or
  recursive. Preparing an upload now fails outright if its destination
  directory can't be listed for any reason other than "doesn't exist yet" —
  a server that refuses `LIST` on a writable directory is surfaced as an
  error rather than silently treated as conflict-free
- **Recursive walks share the connection.** A walk locks the transport only
  for the directory it's currently listing, not for the whole tree, so an
  F5 refresh or a navigation elsewhere doesn't wait for a large recursive
  walk to finish
- **Walk checkpointing** — the transfer plan is written to disk before the
  first job runs; each job is marked `in_progress` when it starts and
  `done` when it finishes. If the session is interrupted, press `r`
  (resume downloads) or `R` (resume uploads) in the Transfers pane to
  re-queue only the jobs that didn't complete. Starting a second batch in
  the same direction while the first is still running appends to that
  batch's checkpoint rather than overwriting it, and `r` / `R` refuses to
  resume while a batch of that direction is still in flight (finish or
  cancel it first).
  Connecting to a session that has an unfinished batch offers to resume it,
  with a summary of what is left. `[r]` resumes, `[d]` discards the
  checkpoint *and* the partial downloads it is the only record of, and
  `[esc]` defers — the offer returns on the next connect. Use `blink
  checkpoints` to inspect pending checkpoints from the command line.
- **Checkpoint keying** — checkpoints are keyed by session *name*, not by
  host. An ad-hoc `blink connect sftp://host` therefore matches a
  checkpoint belonging to a saved session called `host`, and editing a
  saved session's host leaves its old checkpoint in place. Check the paths
  in the summary if that is a possibility.
- **Checkpoint file format** is versioned (currently version 3, which adds
  a `cancelled` job status); checkpoints written by older blink versions
  (version 2) still load and resume normally.
- **Download resume is provenance-checked.** A `.part` file records bytes,
  not which remote file they came from, so blink writes a `<dest>.part.meta`
  sidecar alongside it naming the remote path and the size the server
  reported. Resume only happens when the sidecar identifies the same remote
  file at the same reported size; anything unproven — including a `.part`
  left by a pre-sidecar version of blink — restarts from byte zero instead
  of risking a silently corrupt file.

### Sessions

- Saved sessions in INI files; create, edit, delete from the selector
- Per-session overrides for `local_dir` (with `~` expansion), `remote_dir`,
  `parallel_downloads`, `accept_invalid_certs`, theme
- Passwords are never persisted; prompted at connect time
- Session URL parser (`sftp://user@host:port/path`) for ad-hoc connects
- **`n` connects ad-hoc, it does not save.** Nothing is written to disk until
  you say so — which keeps throwaway connects cheap, but is easy to mistake
  for a failed save. Once the connection comes up blink offers to persist it;
  decline and `ctrl+s` still saves at any point during the session (and
  snapshots your current remote/local directories while it's at it).

### Theming

Seven built-in themes — `dracula`, `aura`, `nord`, `solarized-dark`,
`solarized-osaka`, `tokyo-night`, `cyberpunk-neon` — user-supplied themes
drop in as INI files in the themes directory. **`t`** cycles through
available themes from either the session selector or the main view, with
the choice persisted to `config.ini` automatically.

## Build

### Prerequisites

- **Rust 1.85+** with the 2024 edition

That's it. Every TLS-using dependency is pure Rust (rustls), so there's no
need for `libssl-dev` or any other system TLS library on any platform.

### Build

```sh
cargo build --release
```

The binary lands in `target/release/blink`.

### Static Linux binary (portable, glibc-free)

A normal `cargo build --release` produces a binary dynamically linked
against your build machine's glibc. Running it on a host with an older
glibc fails with errors like `version 'GLIBC_2.39' not found` — common on
appliances such as TrueNAS SCALE, older Debian/Ubuntu, or any distro you
don't control.

Because every dependency is pure Rust, blink builds cleanly against musl
into a **fully static binary with zero libc dependency** that runs on any
x86-64 Linux regardless of its glibc version.

```sh
# One-time setup
rustup target add x86_64-unknown-linux-musl
# Debian / Ubuntu
sudo apt install musl-tools
# Fedora / RHEL
sudo dnf install musl-gcc musl-libc-static

# Build
cargo build --release --target x86_64-unknown-linux-musl
```

Output: `target/x86_64-unknown-linux-musl/release/blink` — a static-PIE
executable (`ldd` reports `statically linked`).

The musl C compiler (`musl-gcc`) is required because a couple of crypto
dependencies (`aws-lc-rs`, `ring`) compile C code. The repo's
`.cargo/config.toml` already points the musl target at `musl-gcc` and sets
the matching `CC`, so no environment variables are needed — the command
above works as-is and leaves your normal `cargo build` for local dev
untouched.

### Cross-platform notes

- Linux and Windows are the two officially-tested targets
- macOS should work but hasn't seen as much exercise

### Cross-compiling for Windows from Linux

You can produce a Windows binary without leaving your Linux box. There are
two routes; pick based on what's already on your system.

#### Option A — `cargo-xwin` (recommended, no Wine needed)

[`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) downloads the
Microsoft CRT and Windows SDK on first use, so you don't need Wine or a
licensed Visual Studio. Best route on a clean Linux box.

```sh
# One-time setup
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin

# Build
cargo xwin build --release --target x86_64-pc-windows-msvc
```

Output: `target/x86_64-pc-windows-msvc/release/blink.exe`.

The first build downloads ~700 MB of SDK headers and libs into
`~/.cache/cargo-xwin/`; subsequent builds reuse the cache.

#### Option B — MinGW-w64 (`x86_64-pc-windows-gnu`)

If you'd rather use the GNU toolchain (no Microsoft CRT), MinGW-w64
works for blink because nothing in the dependency tree needs MSVC-only
features.

```sh
# Debian / Ubuntu
sudo apt install mingw-w64
# Fedora / RHEL
sudo dnf install mingw64-gcc

rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/blink.exe`.

#### Notes on cross-compiled binaries

- The Windows binary is a real PE32+ executable; it runs natively on
  Windows 10 / 11 with no runtime dependencies beyond the standard
  Microsoft Visual C++ Runtime (already present on every modern Windows).
- For ARM64 Windows, swap `x86_64` for `aarch64` in either route.

## Run

```sh
# Launch into the session selector
blink

# Connect directly without a saved session
blink connect sftp://user@host:22

# Open a saved session by name
blink open production

# Print built-in themes
blink themes

# List saved sessions
blink sessions

# Show any interrupted batch-transfer checkpoints
blink checkpoints

# Remove completed and orphaned checkpoint files
blink checkpoints --clean

# Remove all checkpoint files unconditionally
blink checkpoints --force
# (both also delete the .part files, and their .part.meta sidecars, left
#  by the batches they remove)

# Forget a stored SSH host key (see "known_hosts" below before running this)
blink known-hosts remove host.example.com
blink known-hosts remove host.example.com --port 2222
```

`blink open` exits with an error if the session name is not found.
`blink connect` accepts any URL in the form `protocol://[user@]host[:port][/path]`
where protocol is one of `sftp`, `scp`, `ftp`, or `ftps`. Both commands
prompt for a password if the session uses password auth, or go straight to
the Connection screen for key and agent auth.

Omitting the port picks the protocol default: **22** for `sftp` / `scp`,
**21** for `ftp` *and* `ftps`. FTPS defaults to 21 rather than 990 because
blink speaks explicit FTPS (`AUTH TLS` on the standard FTP port); see
[Honest caveats](#honest-caveats). Implicit-mode servers need `:990` spelled
out, and blink cannot currently talk to them anyway.

## Configuration

Files live in platform-appropriate locations:

| Path          | Linux                                                            | macOS                                                | Windows                                       |
| ------------- | ---------------------------------------------------------------- | ---------------------------------------------------- | --------------------------------------------- |
| Global config | `$XDG_CONFIG_HOME/blink/config.ini` (else `~/.config/blink/`) | `~/Library/Application Support/blink/config.ini`  | `%USERPROFILE%\Documents\blink\config.ini`   |
| Sessions      | `~/.config/blink/sessions/`                                       | `~/Library/Application Support/blink/sessions/`       | `%USERPROFILE%\Documents\blink\sessions\`     |
| Themes        | `~/.config/blink/themes/`                                         | `~/Library/Application Support/blink/themes/`         | `%USERPROFILE%\Documents\blink\themes\`       |
| Known hosts   | `~/.config/blink/known_hosts`                                     | `~/Library/Application Support/blink/known_hosts`     | `%USERPROFILE%\Documents\blink\known_hosts`   |
| Checkpoints   | `~/.config/blink/checkpoints/`                                    | `~/Library/Application Support/blink/checkpoints/`    | `%USERPROFILE%\Documents\blink\checkpoints\`  |

macOS honours `XDG_CONFIG_HOME` if explicitly set; otherwise it follows the
platform's `Library/Application Support` convention.

### `known_hosts` — SSH host key store

blink maintains its own known-hosts file separate from `~/.ssh/known_hosts`.
The on-disk format matches OpenSSH: one entry per line, `hostname key-type
base64-key`. The hostname is stored as a bare lowercased host for the
default SSH port and `[host]:port` for any other port — exactly as OpenSSH
writes it. Hashed (`|1|salt|hash`) entries from `HashKnownHosts=yes` aren't
supported; blink only reads files it wrote itself. Files written by older
blink versions in the `host:port` form are still accepted on lookup, so
upgrades don't force re-verification.

Match semantics also follow OpenSSH: a host with both an ed25519 and an
rsa entry is normal multi-algorithm behaviour, not a mismatch. The
"changed key" hard-reject fires only when the *same* keytype on file
has a different key blob.

When connecting via SFTP or SCP for the first time, blink shows a prompt
with the server's SHA-256 fingerprint and three choices:

| Key | Action |
| --- | ------ |
| `y` | Accept and save to `known_hosts` (future connects are silent) |
| `t` | Trust once — accept for this session only, don't save |
| `n` / Esc | Reject — abort the connection |

"This session" for `t` means the whole connected session, not just the
connection the prompt interrupted: every connection the session opens
afterwards — including each parallel transfer worker, which opens its own
connection — honours the same accept-once decision instead of re-prompting.
The trust is forgotten as soon as the session disconnects.

If a host's key changes after being saved, blink hard-rejects the
connection and shows a warning screen that only `Enter` / `Esc` / `q`
dismisses (so a held key can't blow past the warning). Dismissing it tears
down the connection — including a session that was already connected and
transferring, since a worker's own connection can trip this mid-session —
and returns to the session selector rather than leaving a live session
whose peer just failed to prove its identity. The screen prints
the exact command that clears the stored entry:

```sh
blink known-hosts remove host.example.com            # port 22
blink known-hosts remove host.example.com --port 2222
```

That command is deliberately *not* a key on the warning screen. A key
mismatch is what a legitimate key rotation and a man-in-the-middle look
like alike, so forgetting the old fingerprint should take a conscious step
outside the TUI — the same reason OpenSSH points you at `ssh-keygen -R`
instead of offering to do it for you. Confirm the new fingerprint through
some channel other than the connection that just failed before you run it.

Removal takes every algorithm stored for that host and port (a host with
both an `ssh-ed25519` and an `ssh-rsa` entry is fully forgotten), leaves
other hosts and comments untouched, and reports how many entries went
away — `0` almost always means the host was stored under a different port.

Concurrent appends from two blink processes accepting the same new host
are serialised through an exclusive advisory file lock, so two parallel
connects can't write duplicate or interleaved lines.

### `config.ini` — global

```ini
[general]
theme = dracula             ; one of the seven built-ins, or a user theme
parallel_downloads = 2      ; default; sessions can override (max 10)
confirm_quit = true

[terminal]
image_preview = auto        ; auto | kitty | sixel | iterm2 | none
```

An out-of-range `parallel_downloads` — `0`, or anything above the maximum —
is clamped into range rather than rejected, in `config.ini` and in the
per-session `[transfer]` override alike, so a hand-edited or stale file
never stops blink from starting. A value that is not a number at all has no
sensible correction, and the two files differ deliberately: `config.ini`
rejects it outright, while a session file ignores the override and falls
back to the global setting rather than locking you out of that host.

### `sessions/<name>.ini` — per session

```ini
[session]
name = production
protocol = sftp             ; sftp | scp | ftp | ftps
host = prod.example.com
port = 22
username = me
remote_dir = /var/www
local_dir = ~/work/prod     ; optional override; ~ expands

[auth]
method = key                ; password | key | agent
key_path = ~/.ssh/id_ed25519

[transfer]
parallel_downloads = 4      ; optional override

[appearance]
theme = tokyo-night         ; optional override

[tls]
accept_invalid_certs = false  ; FTPS only; default false. true switches
                              ; from CA-chain validation to TOFU pinning:
                              ; hostname is still verified, the handshake
                              ; signature is still verified, and the
                              ; cert SHA-256 is pinned on first connect.
cert_sha256 = abc123…         ; auto-populated by the TOFU pin above; do
                              ; not edit by hand. Clear it (delete the
                              ; line) if the legitimate cert rotates.
```

Passwords are never written to disk; in-memory copies are wiped with
`zeroize` when the connected session ends.

### `themes/<name>.ini` — user themes

```ini
[theme]
name = my-theme

[colors]
bg              = #1a1b26
fg              = #c0caf5
dim             = #565f89
cursor_bg       = #282a36
border_active   = #bb9af7
border_inactive = #292e42
accent          = #f7768e
directory       = #7dcfff
image           = #f7768e
selected        = #e0af68
success         = #9ece6a
warning         = #ff9e64
error           = #f7768e
```

**The filename is the identifier, not the `[theme] name` field.** A theme is
loaded by its *key* — a built-in name, or a user file's stem — which is what
goes in `config.ini` and what `t` cycles through. `[theme] name` is only the
label shown in the status bar, and it is optional; leave it out and the stem
is used for both. The two are allowed to differ, so `custom.ini` can present
itself as `My Cool Theme`, but `theme = custom` is what selects it.

## Hotkeys

The full list lives in the in-app help overlay (`?`). Highlights:

| Key            | Action                                       |
| -------------- | -------------------------------------------- |
| `tab` / `S-tab`| cycle active pane (Local → Remote → Transfers → Log) |
| `↑` / `↓`      | move cursor                                  |
| `↵`            | open file or enter directory                 |
| `backspace`    | go up to parent directory                    |
| `space`        | select / deselect                            |
| `^d`           | download selected items                      |
| `^u`           | upload selected items                        |
| `v`            | view image or text                           |
| `/`            | filter current pane                          |
| `F5`           | refresh active pane                          |
| `F2`           | rename (remote pane)                         |
| `F7`           | create new remote directory                  |
| `S-del` / `D`  | delete file or folder (remote pane)          |
| `^s`           | save current session                         |
| `^x`           | disconnect (return to selector)              |
| `t`            | cycle theme                                  |
| `c`            | cancel selected transfer (Transfers pane)    |
| `C`            | cancel whole batch (Transfers pane)          |
| `r`            | resume interrupted download batch (Transfers pane) |
| `R`            | resume interrupted upload batch (Transfers pane)   |
| `p`            | pause / resume all transfers                 |
| `?`            | toggle help                                  |
| `q` / `esc`    | quit (with confirmation)                     |

In the session selector: `n` new, `e` edit, `d` delete, `t` cycle theme.

## Supported viewer formats

`v` opens the in-app viewer for the cursor item. Whether a file is recognised
depends on its extension (or, for a few well-known names, the bare filename).
False negatives just mean you have to download to read it; nothing is
guessed by content sniffing.

### Images (any of the three supported terminal graphics protocols)

`.png`, `.jpg` / `.jpeg`, `.gif`, `.webp`

Display caps at 25 MB. That bounds the fetch, not the decode: the decoder
independently refuses anything over 4096 px per side or 128 MiB of
allocation, checked from the image header before a pixel buffer is
allocated, so a small file declaring huge dimensions is still rejected.

### Text

Display caps at 1 MB.

**Bare filenames recognised without an extension:**
`README`, `LICENSE` / `LICENCE`, `Makefile`, `Dockerfile`, `CHANGELOG`,
`AUTHORS`, `CONTRIBUTORS`, `TODO`, `NOTICE`

**Recognised extensions, by category:**

| Category    | Extensions                                                    |
| ----------- | ------------------------------------------------------------- |
| Generic     | `txt`, `md`, `rst`, `log`                                     |
| Config      | `ini`, `conf`, `cfg`, `config`, `env`, `gitignore`, `gitattributes`, `editorconfig` |
| Data        | `json`, `yaml`, `yml`, `toml`, `xml`, `csv`, `tsv`            |
| Web         | `html`, `htm`, `css`, `scss`, `sass`, `less`                  |
| JavaScript  | `js`, `mjs`, `cjs`, `ts`, `jsx`, `tsx`                        |
| Systems     | `rs`, `c`, `h`, `cpp`, `cxx`, `cc`, `hpp`, `go`               |
| Scripting   | `py`, `rb`, `lua`, `pl`, `r`, `php`                           |
| JVM/.NET    | `java`, `kt`, `swift`, `cs`                                   |
| Shell       | `sh`, `bash`, `zsh`, `fish`, `ps1`, `bat`                     |
| Database    | `sql`                                                         |
| Patches     | `diff`, `patch`                                               |
| Release     | `nfo`                                                         |

Text decoding: NFO files are decoded as CP437 (DOS codepage 437), preserving
box-drawing characters. All other text files are decoded as UTF-8 (lossy —
unrecognised byte sequences render as replacement characters rather than
failing the viewer).

If you'd like another extension recognised, the allowlist is one match arm
in `src/preview.rs::is_viewable_text`.

## Architecture

```
src/
├── main.rs              entrypoint, CLI parsing, terminal lifecycle
├── error.rs             one error enum to rule them all
├── paths.rs             platform config / session / theme / checkpoint dirs; sync_parent_dir helper
├── config.rs            global config.ini load / save (preserves unknown keys)
├── session.rs           per-session .ini load / save / list / URL parser
├── theme.rs             theme model + 7 built-ins + file loader
├── checkpoint.rs        walk-plan checkpointing: persist, append, remove batch state; debounced fsync writes; version-3 format with v1/v2 migration
├── known_hosts.rs       SSH host-key store: check / append / remove, OpenSSH-style matching, file lock, SessionTrust (per-session "trust once")
├── highlight.rs         syntax highlighter for the text viewer (single-pass, zero deps)
├── transport/           connection layer
│   ├── mod.rs           Transport trait + factory + Connected struct + part_path / part_meta_path + resume provenance (decide_resume)
│   ├── sftp.rs          SFTP via russh + russh-sftp (SSH keepalive, rsa-sha2-512)
│   ├── scp.rs           transparent SFTP wrapper (matches OpenSSH 9.0+); delegates via the delegate_inner_transport! macro
│   ├── ftp.rs           FTP via suppaftp tokio backend
│   ├── ftps.rs          FTPS via suppaftp + rustls; pinning verifier (hostname + signature + cert pin)
│   ├── ftp_impl.rs      shared macro that generates the Transport impl for FTP and FTPS
│   └── error_map.rs     maps russh-sftp / suppaftp errors to typed BlinkError variants (NotFound / Permission / Disconnected)
├── transfer.rs          TransferManager: queue, state, progress events; MAX_QUEUED_JOBS cap
├── transfer/
│   └── dispatcher.rs    parallel slot dispatcher; per-job worker tasks; zeroized password
├── preview.rs           kitty / sixel / iterm2 backends; FileViewKind classification; CP437 NFO decoder
└── tui/
    ├── mod.rs           terminal init / restore + top-level run wrappers
    ├── event.rs         keyboard / app-event multiplexer
    ├── plan.rs          recursive walk planner: PlannedJob, WalkResult, walk_remote / walk_local, conflict probes
    ├── state.rs         per-modal / per-pane / per-form state types (PaneState, Viewer, PendingHostKey, EditSessionForm, …)
    ├── views.rs         render functions per screen / overlay
    ├── widgets.rs       file pane, bottom panel, status bar, transfer strip
    └── app/             the App state machine, split by responsibility
        ├── mod.rs       struct + new / with_session / run + draw + handle_key dispatcher + connect / disconnect / push_log
        ├── handlers.rs  per-screen key handlers (one impl App block, all handle_* methods)
        ├── events.rs    handle_app_event + handle_transfer_event (background-task drain)
        ├── checkpoint_glue.rs  dispatch_plan / resume_walk / settle_checkpoint
        ├── transfers.rs orchestration: enqueue / walk-spawn / confirm overwrite → dispatch
        ├── actions.rs   modal openers + submitters + async kickoffs (rename / mkdir / delete / edit-session / search / save-session)
        ├── panes.rs     pane navigation, cursor, refresh (refresh_local_pane runs on the blocking pool)
        ├── controls.rs  cancel / pause / theme controls + active_jobs snapshot
        └── viewer.rs    file viewer (text + image), tokenisation cache, after_draw image-redraw hook
```

The trait boundaries that matter for extension:

- **`Transport`** in `transport/mod.rs` — implement this once per protocol.
  Adding a new protocol is one new file plus one match arm in
  `transport::open`.
- **`PreviewBackend`** in `preview.rs` — implement once per terminal-graphics
  protocol.
- **`Theme`** is just a struct loaded from INI; new themes drop in as files,
  no code changes needed.

The transfer layer has its own clean seam: `TransferManager` owns the queue
and progress channel; `Dispatcher` is a separate task that pulls pending
jobs and runs them against `Box<dyn Transport>`. The two communicate only
through the manager's public methods, so the dispatch policy can be swapped
or extended without touching the queue.

## Security

blink connects to remote servers over the open internet and renders
server-supplied content (filenames, error messages, file previews) in your
terminal. The following properties are enforced in the current codebase.

### Protocol

- **SFTP / SCP host-key verification** — unknown keys trigger an interactive
  prompt; a changed key is a hard rejection with a warning screen that
  only `Enter` / `Esc` / `q` dismisses. Keys are stored in OpenSSH format.
  Matching follows OpenSSH `(host, keytype)` semantics: multi-algorithm
  hosts (an ed25519 *and* an rsa entry) coexist normally; "changed key"
  only fires when the same keytype has a different blob.
- **SFTP RSA auth uses rsa-sha2-512.** ssh-rsa with SHA-1 has been disabled
  by default in OpenSSH 8.8+ (September 2021); blink negotiates the
  modern hash so RSA users don't get an opaque "rejected by server"
  against any current OpenSSH.
- **SSH keepalive at 30 s × 3.** An authenticated session with a dead TCP
  underneath tears down in ~90 s instead of waiting on the OS keepalive
  (which can run into minutes).
- **FTPS** — explicit TLS only (RFC 4217), verified against the Mozilla CA
  bundle via rustls (pure Rust, no system OpenSSL).
- **FTPS hostname + signature verification are mandatory.** Even when
  `accept_invalid_certs = true` bypasses CA-chain trust, the cert's SAN
  must still match the configured hostname and the handshake signature
  must still verify against the cert's public key. The flag also enables
  TOFU pinning: the leaf cert SHA-256 is recorded in the session on the
  first connect; subsequent connects require an exact match.
- **30-second connect timeout** — applied to both the primary connection
  and every parallel worker connection. A server that accepts the TCP
  socket but stalls the handshake cannot pin connections indefinitely.

### Terminal injection prevention

All server-controlled strings pass through a sanitizer before being stored or
rendered. Two classes of character are replaced with spaces:

- **Control characters** (U+0000–U+001F and U+007F–U+009F) — covers ESC and
  every ANSI sequence starter.
- **Bidirectional and zero-width formatters** (U+061C, U+200B–U+200C,
  U+200E–U+200F, U+202A–U+202E, U+2066–U+2069, U+FEFF). These are Unicode
  category **Cf**, so a `is_control()` check alone misses them. A
  RIGHT-TO-LEFT OVERRIDE lets a server name a file that *renders* as
  `report.png` while the bytes end in `.exe`; zero-width characters let two
  different names render identically. Replacing rather than deleting is
  deliberate — it makes the difference visible instead of collapsing two
  distinct names onto one rendering. U+200D ZERO WIDTH JOINER is
  intentionally kept: it can't spoof an extension, and stripping it would
  break emoji sequences in legitimate filenames.

Applied to:

- Remote directory-entry names (`list()` in every transport)
- SSH key-type strings and host-key fingerprints
- Error messages from transport layers
- Text file content in the viewer (tabs preserved; no length cap beyond the
  25 MB transport read limit) — the bidi filter matters as much here, since
  viewing remote source is exactly the "trojan source" setting
- Session and checkpoint names printed by the CLI subcommands

### Path safety

- **Remote-to-local path traversal** — entry names containing `/`, `\`, `\0`,
  or equal to `..` / `.` are rejected before `Path::join` in download paths
  and recursive walks (`safe_local_name()`).
- **Remote path injection** — `join_remote()` strips leading `/` from
  server-supplied names and rejects any `..` component, preventing a server
  from escaping the working directory via path construction.
- **Recursive walks skip symlinks by default.** A server-side symlink
  named `passwd` pointing at `/etc/passwd` won't get fetched into the
  user's destination tree, and an A→B→A symlink cycle can't loop the
  walker. Single-file `View` of a symlink still works — that's an
  explicit per-file action.
- **Recursive uploads skip local file names that aren't valid UTF-8.**
  Sending a lossily-decoded name (`\u{FFFD}` in place of the bad bytes)
  would upload the file under a name that isn't its own and that nothing
  can map back. The skip is counted and reported in the log, the same way
  a skipped symlink is.
- **Directories count against the recursive-walk budget.** A remote
  download walk emits jobs only for files, so a server serving a deep or
  very wide tree of *empty* directories would otherwise expand the walk
  (and create local directories) without ever tripping the job cap.
- **SFTP recursive delete unlinks symlinks rather than following them.**
  Some SFTP servers report symlink-to-directory entries with both
  `is_dir` and `is_symlink` set; a recursive `delete_dir(true)` that
  followed those could delete files outside the chosen subtree.
- **Password-in-URL is rejected.** `sftp://alice:hunter2@host/` errors
  with a pointer at the interactive prompt rather than smuggling the
  password into the username field and into shell history.

### Resource limits

| Resource | Limit |
| -------- | ----- |
| Text file preview read | 1 MB (at preview detection; 25 MB at transport) |
| Image file preview read | 25 MB (at preview detection and transport) |
| Image decoder pre-decode dimension cap | 4096 × 4096 px |
| Image decoder max allocation | 128 MiB (enforced *before* full pixel-buffer alloc) |
| Transfer job queue | 100,000 jobs |
| Recursive walk plan | 100,000 entries — planned jobs *plus* directories visited and queued (bails before materialising the whole tree) |
| Error string length | 512 characters |
| Session / config / theme files | 64 KiB each |
| Checkpoint files | 10 MiB |
| Known-hosts file | 1 MiB |

Transport-layer read caps are enforced independently of server-reported
file sizes, so a server that lies in its directory listing cannot bypass
them. The image decoder limit is set via `image::Limits` so the decoder
refuses oversized declarations on the header rather than after the full
RGBA buffer is already allocated.

### Credential handling

- Passwords and SSH key passphrases are held in memory only for the
  duration of the connected session and are never written to disk.
- In-memory copies are wrapped in `zeroize::Zeroizing<String>` so the
  underlying allocation is wiped when the credential is dropped (cleared
  on disconnect / quit / connect failure). A long-running blink process
  doesn't leave the credential greppable in core dumps after the auth
  window has closed.
- **This covers the prompt buffers too, not just the stored copy.** The
  password and passphrase fields are `Zeroizing` from the first keystroke,
  pre-sized so that typing never reallocates — a growing `String` copies the
  partial secret into a new allocation and frees the old one *without*
  clearing it, stranding fragments that no later wipe can reach. Abandoning
  a prompt zeroizes the buffer rather than calling `clear()`, which would
  only reset the length and leave the bytes in place.
- Each parallel worker slot opens its own authenticated connection and
  receives the cached credentials; no shared state crosses task
  boundaries.

### Config and session file safety

- **A session is validated before it is written.** `Session::save` refuses
  anything `load_from` would reject — a relative `local_dir`, a relative
  `key_path`, a field carrying a newline. Previously the edit-session form
  applied no validation of its own, so typing a relative path into the Local
  dir field saved successfully and produced a file the loader then skipped:
  the session disappeared from the selector on the next launch, `.ini` still
  on disk, with no way to reach it from the UI. Enforcing the invariant in
  `save` means no form can reintroduce that.
- **Session files that fail to load are reported, not silently dropped.**
  `blink sessions` prints a `warning: skipped <file>: <reason>` line to
  stderr, and the TUI logs one per unreadable file at startup and on every
  reload. The old `tracing::warn` went to a sink unless `BLINK_LOG_FILE`
  was set, so the only symptom was a session quietly going missing.
- Session, config, and checkpoint files are written with the full
  atomic-and-durable pattern: tempfile → `sync_all` → rename →
  `sync_all` on the parent directory. Without the syncs, a power loss
  between rename and the filesystem journal commit can leave a
  zero-byte file or roll the rename back. (Unix only; on Windows the
  filesystem journals rename through its own ordering rules.)
- Checkpoint writes during a hot transfer batch are debounced at 250 ms
  so a 100k-job batch doesn't generate ~200k full plan rewrites. Any
  lost mark on a crash just causes the affected job to be re-queued on
  resume — never silently skipped.
- Downloads write to a `<local>.part` sibling and rename onto the final
  name only after `flush` + `sync_all`. The user's existing file (if any)
  isn't truncated until the new download has fsynced cleanly. A
  `<local>.part.meta` sidecar is written alongside it, recording the remote
  path and the size the server reported — the provenance a resume needs to
  tell "my interrupted download" from "an unrelated file that happens to
  share this local name" (see "download resume is provenance-checked"
  under Transfers).
- Uploads mirror this on the remote side: bytes stream into
  `<remote>.part` and the final name is only created by rename after the
  upload completes (and fsyncs, where the server supports
  `fsync@openssh.com`). SFTP uses `posix-rename@openssh.com` for an
  atomic replace when the server offers it; otherwise (and on FTP/FTPS,
  where overwrite-on-rename is server-dependent) the target is removed
  and the rename retried — that window can expose "old file gone, new
  file still at `.part`", but never a truncated file under the final
  name. An upload interrupted by a hard kill or dropped connection can
  leave a stale `<remote>.part` behind; re-running the upload reuses
  (truncates) it.
- Config directories are created with mode 0700 on Unix (not
  world-readable). A `BLINK_LOG_FILE` is created with mode 0600, since at
  debug level it records hostnames and remote paths; if the file already
  exists with looser permissions blink warns rather than silently
  tightening a path you chose.
- `Config::save` round-trips unknown INI keys, so hand-edited or
  forward-compat options aren't silently dropped on a theme cycle.
- Path-traversal and null-byte validation is applied when loading
  session and theme names from disk.
- Server-supplied filenames are validated before being joined onto a local
  path. Beyond the universal `/`, `\`, `..` and null-byte rejections,
  Windows additionally rejects `:` (a drive prefix like `C:evil` would
  otherwise *replace* the destination path per `PathBuf::push` semantics,
  and `name:stream` opens an alternate data stream), trailing dots and
  spaces (Windows strips them, aliasing two names to one file), and
  reserved device names (`NUL`, `COM1`, …).

## Honest caveats

A few things worth knowing before you use this in anger:

- **FTPS is explicit-only.** The `AUTH TLS` upgrade path (RFC 4217) is what
  every modern server speaks. Implicit FTPS on the deprecated port 990 is
  not supported. If you have a server that only does implicit-mode, you'd
  need a different connect path; the `transport/ftps.rs` seam is small.
- **FTPS uses the embedded Mozilla CA bundle** (`webpki-roots`) for trust
  anchors rather than the system trust store. Self-signed certs and
  privately-rooted CAs aren't trusted by default. The per-session
  `accept_invalid_certs` toggle (visible in the edit-session form)
  switches from CA-chain trust to TOFU pinning: the cert hash is
  recorded on the first connect and subsequent connects require an exact
  match. Hostname binding and handshake-signature verification stay on
  in both modes. There is no option to add a custom CA root without
  recompiling.
- **RSA client keys carry a known timing-sidechannel risk.** The `rsa`
  crate reached transitively through `russh` is affected by
  [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071)
  ("Marvin Attack") and has no fixed release. Exploiting it needs many
  precisely-timed oracle queries against your private key, which a normal
  interactive session does not provide — but if you want the risk gone,
  use an Ed25519 key. The rationale is recorded in `.cargo/audit.toml`,
  the ignore list used when running `cargo audit` against this tree.
- **Passwords are held in memory** for the duration of the connected
  session, but the allocation is zeroised on drop. Each parallel
  transfer slot opens its own connection, so the dispatcher needs
  credentials to handshake each one. If that's not acceptable for your
  threat model, use SSH key auth or ssh-agent instead — the key file
  (or the agent's identity store) stays put and no in-memory copy of
  the secret is needed.
- **Cancellation cascades at both the single-job and batch level.** `c`
  cancels the selected transfer; `C` cancels every active and queued job
  in the same recursive batch. There is no partial-tree cancel (e.g.
  cancelling only a specific subdirectory within a larger walk).
- **Walk checkpoints survive a clean exit *and* most hard kills.** The
  initial plan is written with `flush()` (synchronous) before any job
  runs. Per-job state changes are debounced at 250 ms — a crash within
  that window leaves the affected job in its previous state on resume,
  which is the same safe outcome as a crash mid-transfer (Pending →
  re-queued, InProgress → re-queued, Done → re-run). Partial downloads
  live at `<name>.part`; the final name is only created via rename
  after fsync. `mkdir` is idempotent on the remote side, so re-runs are
  safe across the board. A resumed `.part` is only trusted if its
  `.part.meta` sidecar names the same remote path at the same reported
  size — a `.part` from before this check existed, or one whose sidecar
  didn't survive the crash, restarts instead of risking a silently
  corrupted file.
- **Transfers don't auto-refresh the local pane.** Use F5 to refresh after
  downloads complete.

## License

MIT. See [LICENSE](LICENSE) for the full text.

## Third-party attributions

blink is built on the following open-source libraries. Each is used as an
unmodified dependency; their licenses apply to their respective source code
and do not affect blink's MIT license except where noted.

### MIT

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [ratatui](https://github.com/ratatui/ratatui) | ratatui contributors | TUI layout and rendering |
| [crossterm](https://github.com/crossterm-rs/crossterm) | TimonPost et al. | Cross-platform terminal I/O |
| [tokio](https://github.com/tokio-rs/tokio) | Tokio contributors | Async runtime |
| [tokio-util](https://github.com/tokio-rs/tokio) | Tokio contributors | Async I/O utilities |
| [tracing](https://github.com/tokio-rs/tracing) | Tokio contributors | Structured logging |
| [tracing-subscriber](https://github.com/tokio-rs/tracing) | Tokio contributors | Log sink / filter |
| [bytes](https://github.com/tokio-rs/bytes) | Tokio contributors | Byte buffer utilities |
| [rust-ini](https://github.com/zonyitoo/rust-ini) | Y. T. | INI config parser |
| [icy_sixel](https://github.com/nickel-lang/icy_sixel) | Mike Krüger | Sixel image encoding |
| [parking_lot](https://github.com/Amanieu/parking_lot) | Amanieu d'Antras | Faster synchronisation primitives |

### MIT OR Apache-2.0

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [async-trait](https://github.com/dtolnay/async-trait) | David Tolnay | Async trait support |
| [futures](https://github.com/rust-lang/futures-rs) | Alex Crichton et al. | Future combinators |
| [suppaftp](https://github.com/veeso/suppaftp) | Christian Visintin | FTP / FTPS client |
| [tokio-rustls](https://github.com/rustls/tokio-rustls) | rustls contributors | Async TLS via rustls |
| [serde](https://github.com/serde-rs/serde) | David Tolnay, Erick Tryzelaar | Serialisation framework |
| [serde_json](https://github.com/serde-rs/json) | David Tolnay, Erick Tryzelaar | JSON serialisation (checkpoints) |
| [directories](https://github.com/dirs-dev/directories-rs) | Simon Ochsenreither | Platform config-dir paths |
| [clap](https://github.com/clap-rs/clap) | clap contributors | CLI argument parsing |
| [thiserror](https://github.com/dtolnay/thiserror) | David Tolnay | Error derive macro |
| [image](https://github.com/image-rs/image) | image-rs contributors | Image decoding (PNG, JPEG, GIF, WebP) |
| [base64](https://github.com/marshallpierce/rust-base64) | Marshall Pierce et al. | Base64 encoding for image preview |
| [chrono](https://github.com/chronotope/chrono) | chronotope contributors | Date / time formatting |
| [sha2](https://github.com/RustCrypto/hashes) | RustCrypto contributors | SHA-256 for known-hosts disambiguation and FTPS cert pins |
| [zeroize](https://github.com/RustCrypto/utils/tree/master/zeroize) | RustCrypto contributors | Wipe in-memory passwords on drop |

### Apache-2.0

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [russh](https://github.com/Eugeny/russh) | Eugeny, Pierre-Étienne Meunier | SSH transport (SFTP / SCP) |
| [russh-sftp](https://github.com/Eugeny/russh) | Eugeny | SFTP protocol layer |

### Mozilla Public License 2.0 (MPL-2.0)

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [webpki-roots](https://github.com/rustls/webpki-roots) | Mozilla / rustls contributors | Mozilla CA root certificates for FTPS |
