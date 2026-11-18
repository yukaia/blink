# blink

A cross-platform terminal SFTP / SCP / FTP / FTPS client with a three-pane TUI, built using Claude.

![rust](https://img.shields.io/badge/rust-2024%20edition-orange)
![license](https://img.shields.io/badge/license-MIT-blue)

![blink](images/main-menu.jpg)

Release notes live in [CHANGELOG.md](CHANGELOG.md).

## Features

### Connectivity

- **SFTP** with password, SSH key, and encrypted-key (passphrase prompt) auth
- **SCP** as transparent SFTP — matches OpenSSH 9.0+ behavior, full feature parity
- **FTP** with anonymous and password auth — *unencrypted*, and blink says
  so on every connect (see [Security](#protocol)). Prefer FTPS or SFTP
  where the server offers them.
- **FTPS** with explicit TLS (RFC 4217) via rustls — pure Rust, no system
  TLS library needed
- **ssh-agent** auth on Unix (uses `$SSH_AUTH_SOCK`); on Windows, the
  built-in OpenSSH agent (`\\.\pipe\openssh-ssh-agent`) is tried first,
  falling back to Pageant
- **Host-key verification** for SFTP/SCP — unknown keys trigger an
  interactive prompt (accept & save / trust once / reject); changed keys are
  hard-rejected with a clear warning. Keys are stored in blink's own
  `known_hosts`, in OpenSSH's line format; see
  [`known_hosts`](#known_hosts--ssh-host-key-store) for what is and isn't
  read from it.

### Browsing & file operations

- Three-pane TUI (local / remote, plus a switchable transfers/log panel)
- Recursive **download** and **upload** with parallel slot dispatch
- **Rename**, **create directories**, **delete files**, **delete directories** (recursive)
- **Substring filter** per pane (`/`), persists across refresh — selections
  made while filtered survive clearing or narrowing the filter too; the
  footer's selection count and size cover the whole selection, including
  entries the filter is currently hiding
- **Refresh** active pane (`F5`) — the current listing stays on screen
  while the new one is fetched
- **Disconnect** and return to the session selector without quitting
  (`Ctrl-X`, then confirm)
- **View** text files inline (scrollable, line-numbered, syntax-highlighted) —
  control and ANSI escape characters are stripped before display to prevent
  terminal injection; see [Supported viewer formats](#supported-viewer-formats)
  for the recognised extensions
- **View** images via [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/),
  [sixel](https://en.wikipedia.org/wiki/Sixel), and iTerm2 inline images —
  auto-detected, aspect-preserving, terminal-cell-aware scaling, and
  photos turned the way their EXIF orientation says

### Transfers

- Parallel slot dispatcher (configurable globally; per-session override)
- **Pipelined SFTP streams** — each transfer keeps multiple read/write
  requests in flight at once instead of waiting a full round-trip per
  chunk, so a single stream is bandwidth-bound rather than latency-bound
  (the way OpenSSH's own `sftp` client works). This stacks on top of the
  parallel slot dispatcher for high aggregate throughput
- Live transfer strip with bytes, percentage, MiB/s
- **Pause / resume** all transfers (`p`)
- **Cancel** individual in-flight transfers (`c`)
- **Cancel whole batch** for recursive transfers (`C`) — aborts every
  active and queued job from the same `Ctrl-D` / `Ctrl-U`
- **Overwrite confirmation** with three choices — overwrite all / skip
  conflicts / cancel — for both downloads and uploads, single-file or
  recursive. Preparing an upload fails outright if its destination
  directory can't be listed for any reason other than "doesn't exist yet" —
  a server that refuses `LIST` on a writable directory is surfaced as an
  error rather than silently treated as conflict-free. Likewise, uploads
  into an FTP directory whose listing couldn't be fully read count as
  possible conflicts, since a file of that name may be among the unread
  lines; the log says so before the prompt
- **Batches report how they ended.** A batch finishes with one log line:
  how many files failed (and the key that retries them), or that all of
  them completed. A failure's own `failed:` line can scroll out of the log
  during a large batch; the summary comes last.
- **Folders before their contents.** A recursive upload creates each remote
  folder before anything is uploaded into it, even with several transfers
  running in parallel.
- **One job per destination.** A download is not queued while another
  queued or running download writes the same local file (compared ignoring
  case on Windows and macOS), and an upload likewise for its remote path —
  two jobs on one destination would corrupt it. The log counts what was
  skipped, and names any file skipped because another in the same batch
  lands on the same path
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
  Once a batch has finished, `r` / `R` re-queues just the files that failed,
  without reconnecting.
  Connecting to a session that has an unfinished batch offers to resume it,
  with a summary of what is left. `[r]` resumes, `[d]` discards the
  checkpoint *and* the partial downloads it is the only record of, and
  `[esc]` defers — the offer returns on the next connect. Use `blink
  checkpoints` to inspect pending checkpoints from the command line.
- **Checkpoint keying** — checkpoints are keyed by session *name* and by
  the server and account the session connects to (`user@host:port`). A
  checkpoint is only offered to, resumed by, or extended by a connection
  to the same account on the same server. An ad-hoc `blink connect
  sftp://alice@host` shares one with a saved session called `host` only if
  that session also connects as `alice` to the same host and port. Editing
  a saved session's user, host or port leaves its old checkpoint behind:
  it is no longer offered, and `blink checkpoints --clean` removes it as
  orphaned (see [Run](#run)). Renaming a session, in the edit form or by
  saving it under a new name with `ctrl+s`, takes its checkpoints along.
- **A cancelled transfer stays cancelled.** Whether you cancel one file
  (`c`) or a whole batch (`C`), the cancelled jobs are never offered for
  resume or re-queued by `r` / `R`.
- **Checkpoint file format** is versioned (currently version 4, which
  records the server and account each checkpoint belongs to). Files from
  versions 1–3 record no server, so they are never offered for resume;
  `blink checkpoints` still lists them, and `--clean` removes them too.
- **Partial files have a name of their own.** In-flight transfers write
  `<name>.<hash>.blink-part`, locally for a download and on the server for
  an upload, where `<hash>` is 8 hex digits of the SHA-256 of `<name>`. So
  blink never touches a `<name>.part` that a browser or another tool left
  beside the file, nor a file that is itself called `<name>.blink-part`. A
  name too long to take the suffix within the usual 255-byte limit is
  shortened in the partial's name only; the hash still tells it apart. The
  final name appears only by rename once the transfer is complete; see
  [Config and session file safety](#config-and-session-file-safety).
- **Download resume is provenance-checked.** A partial download records
  bytes, not which remote file they came from, so blink writes a
  `<name>.<hash>.blink-part.meta` sidecar alongside it naming the server
  and account (`user@host:port`), the remote path, and the size and
  modification time the server reported. Resume only happens when the
  sidecar identifies the same remote file on the same server with the same
  reported size and modification time; anything unproven, including a
  missing sidecar or a server that doesn't report both, restarts from byte
  zero instead of risking a silently corrupt file.

### Sessions

- Saved sessions in INI files; create, edit, delete from the selector
- Per-session overrides for `local_dir` (with `~` expansion), `remote_dir`,
  `parallel_downloads`, `accept_invalid_certs`, theme
- Passwords are never persisted; prompted at connect time (see
  [Credential handling](#credential-handling))
- Session URL parser (`sftp://user@host:port/path`) for ad-hoc connects
- **`n` connects ad-hoc, it does not save.** Nothing is written to disk until
  you say so — which keeps throwaway connects cheap, but is easy to mistake
  for a failed save. Once the connection comes up blink offers to persist it;
  decline and `Ctrl-S` still saves at any point during the session (and
  snapshots your current remote/local directories while it's at it).

### Theming

Seven built-in themes — `dracula`, `aura`, `nord`, `solarized-dark`,
`solarized-osaka`, `tokyo-night`, `cyberpunk-neon` — user-supplied themes
drop in as INI files in the themes directory. **`t`** cycles through
available themes from either the session selector or the main view, with
the choice persisted to `config.ini` automatically. A theme file that
won't load is reported in the log and skipped.

Themes are 24-bit colour. When the terminal doesn't say it supports that
(`COLORTERM=truecolor` or `24bit`, or a terminal known to, such as kitty,
WezTerm, foot, Alacritty, Ghostty or iTerm2), each colour is shown as its
nearest in the 256-colour palette instead. If yours supports 24-bit colour
but isn't recognised, set `COLORTERM=truecolor`.

## Build

### Prerequisites

- **Rust 1.98+** with the 2024 edition. That is the toolchain blink is built
  and tested on, and it is deliberately the number in the manifest: it is the
  only floor anyone has actually verified.

  The *declared* floors underneath are lower — `quantette`, reached through
  `icy_sixel` for sixel image preview, declares 1.90, and blink's own source
  needs 1.89, where `File::lock` stabilised for atomic accept-and-save in
  `known_hosts`. But a declared `rust-version` only records what a crate
  claims, not what it compiles at, and nothing here has ever been built below
  1.98.

  If you need a lower floor, it is one `cargo +<version> check --all-targets`
  away from being real rather than assumed.

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

The musl C compiler (`musl-gcc`) is required because `ring`, the crypto
backend, compiles C code. The repo's `.cargo/config.toml` already points the
musl target at `musl-gcc` and sets the matching `CC`, so no environment
variables are needed — the command above works as-is and leaves your normal
`cargo build` for local dev untouched.

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
features, and nothing needs an external assembler either: `ring` is the
only crypto backend (see [Security](#protocol)), which keeps `aws-lc-sys`
and its NASM requirement out of the graph. `mingw-w64` below is the only
system package required.

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

- The Windows binary is a real PE32+ executable and runs natively on
  Windows 10 / 11. The MinGW build imports only DLLs that ship with Windows
  (`msvcrt.dll` among them). The `cargo-xwin` (MSVC) build links the Visual
  C++ runtime (`vcruntime140.dll`) dynamically, as Rust does by default;
  Windows itself does not ship it, so a machine without the Visual C++
  Redistributable needs it installed, or build with
  `RUSTFLAGS="-C target-feature=+crt-static"` to link it statically.
- For ARM64 Windows, swap `x86_64` for `aarch64` in either route.

## Run

```sh
# Launch into the session selector
blink

# Connect directly without a saved session
blink connect sftp://user@host:22

# Open a saved session by name
blink open production

# List themes: built-ins, and your own marked (user)
blink themes

# List saved sessions
blink sessions

# Show any interrupted batch-transfer checkpoints
blink checkpoints

# Remove completed and orphaned checkpoint files
blink checkpoints --clean
# (a checkpoint counts as orphaned when no saved session has its name and
#  server — its user, host and port. A checkpoint from an ad-hoc
#  `blink connect` is kept: its session is named after the host, and
#  connecting the same way again offers it. If
#  the session list itself can't be read — an unreadable sessions directory,
#  or a session file that won't parse — orphan detection is skipped and a
#  warning is printed, so `--clean` removes only completed checkpoints
#  rather than deleting resume data it can't vouch for)

# Remove all checkpoint files unconditionally, including ones that can't
# be read and temporary files left by an interrupted write
blink checkpoints --force
# (both also delete the .blink-part files, and their .meta sidecars, left
#  by the batches they remove)

# Forget a stored SSH host key (see "known_hosts" below before running this)
blink known-hosts remove host.example.com
blink known-hosts remove host.example.com --port 2222
```

`blink open` exits with an error if the session name is not found.
`blink connect` accepts any URL in the form `protocol://[user@]host[:port][/path]`
where protocol is one of `sftp`, `scp`, `ftp`, or `ftps`. A username may
itself contain `@` — `ftp://me@example.com@ftp.example.net` logs in as
`me@example.com` — since the host is whatever follows the last one. Both commands
prompt for a password if the session uses password auth, or go straight to
the Connection screen for key and agent auth.

Omitting the port picks the protocol default: **22** for `sftp` / `scp`,
**21** for `ftp` *and* `ftps`. FTPS defaults to 21 rather than 990 because
blink speaks explicit FTPS (`AUTH TLS` on the standard FTP port). Implicit
FTPS on port 990 is not supported; see [Honest caveats](#honest-caveats).

### Logging

Logs are discarded unless `BLINK_LOG_FILE` names a file to append them to.
`BLINK_LOG` sets the level (default `warn`) and takes `tracing` filter
syntax:

```sh
BLINK_LOG_FILE=/tmp/blink.log BLINK_LOG=debug blink
```

At debug level the file records hostnames, usernames and remote paths. On
Unix it is created with mode 0600; if it already exists with looser
permissions blink warns rather than silently tightening a path you chose.

## Configuration

Everything lives in one directory, in a platform-appropriate location:

| Platform | Config directory |
| -------- | ---------------- |
| Linux    | `$XDG_CONFIG_HOME/blink/`, else `~/.config/blink/` |
| macOS    | `~/Library/Application Support/blink/` (`$XDG_CONFIG_HOME/blink/` if that variable is explicitly set) |
| Windows  | `%APPDATA%\blink\` |

Inside it:

| Path           | Contents |
| -------------- | -------- |
| `config.ini`   | Global config |
| `sessions/`    | Saved sessions, one `.ini` each |
| `themes/`      | User themes, one `.ini` each |
| `known_hosts`  | SSH host keys |
| `checkpoints/` | Interrupted batch-transfer state |

### `known_hosts` — SSH host key store

blink maintains its own known-hosts file separate from `~/.ssh/known_hosts`.
The on-disk format matches OpenSSH: one entry per line, `hostname key-type
base64-key`. The hostname is stored as a bare lowercased host for the
default SSH port and `[host]:port` for any other port — exactly as OpenSSH
writes it. Comma-separated host lists and `@revoked` lines are read as
OpenSSH reads them, and a revoked key is refused for every host; hashed
(`|1|salt|hash`) entries from `HashKnownHosts=yes` and wildcard host
patterns are not, and `@cert-authority` lines are ignored — a server
presenting an SSH host certificate is refused rather than pinned (see
[Security](#protocol)). Files written by older
blink versions in the `host:port` form are still accepted on lookup, so
upgrades don't force re-verification.

Match semantics also follow OpenSSH: a host with both an ed25519 and an
rsa entry is normal multi-algorithm behaviour, not a mismatch. When
connecting, blink asks first for the keytypes it has on file for the host,
so a server holding several keys proves itself with one blink can check.
The "changed key" hard-reject fires when the keytype on file has a
different key blob, and also when a known host presents only a keytype
that isn't on file — a server that still had its stored key would have
used it, so a new type in its place is treated as a changed key, not a
first connection. If a server really did drop its old key type, forget
the host (`blink known-hosts remove <host>`, above) and accept the new
key.

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
the exact `blink known-hosts remove <host> [--port <port>]` command that
clears the stored entry.

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
; one of the seven built-ins, or a user theme (falls back to dracula,
; with the reason in the log, if it won't load)
theme = dracula
; default; sessions can override (max 10)
parallel_downloads = 2
confirm_quit = true

[terminal]
; auto | kitty | sixel | iterm2 | none
; auto: kitty for kitty and ghostty; iterm2 for iTerm2 and WezTerm; sixel for
; Konsole, foot, mlterm and Windows Terminal. Inside tmux or screen, which
; don't pass images on, auto picks none. xterm draws sixel only when started
; for it (-ti vt340), so it is never guessed: set sixel for one that does.
image_preview = auto
; false: no mouse capture — the terminal's own text selection works, and
; blink ignores the mouse
mouse = true
```

Comments go on lines of their own: a `;` after a value is read as part of
the value.

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
; sftp | scp | ftp | ftps
protocol = sftp
host = prod.example.com
port = 22
username = me
remote_dir = /var/www
; optional override; ~ expands
local_dir = ~/work/prod

[auth]
; password | key | agent
method = key
; an absolute path
key_path = /home/me/.ssh/id_ed25519

[transfer]
; optional override
parallel_downloads = 4

[appearance]
; optional override
theme = tokyo-night

[tls]
; FTPS only; default false. true switches from CA-chain validation to
; trust-on-first-use pinning (see Security).
accept_invalid_certs = false
; With accept_invalid_certs = true, blink adds the pinned certificate's
; SHA-256 here when you accept it and choose to save, as
; cert_sha256 = <64 hex digits>. Do not
; edit it by hand; delete the line if the legitimate certificate rotates.
```

There is no password key: passwords are never written to disk.

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
error           = #db4b4b
```

Every colour is optional: any you leave out is taken from `dracula`. A
colour that's present must be `#rrggbb` (the `#` is optional).

**The filename is the identifier, not the `[theme] name` field.** A theme is
loaded by its *key* — a built-in name, or a user file's stem — which is what
goes in `config.ini` and what `t` cycles through. `[theme] name` is only the
label shown in the status bar, and it is optional; leave it out and the stem
is used for both. The two are allowed to differ, so `custom.ini` can present
itself as `My Cool Theme`, but `theme = custom` is what selects it.

Names are matched ignoring case, with spaces read as hyphens, for built-ins
and user themes alike: `Tokyo Night` is `tokyo-night`, and `My Theme.ini`
is selected by `my-theme`. A user theme with a built-in's name replaces it.
A file whose name contains `:` or `..` can't be selected and is skipped
(`blink themes` says so).

## Hotkeys

The full list lives in the in-app help overlay (`?`). Highlights:

| Key            | Action                                       |
| -------------- | -------------------------------------------- |
| `Tab` / `Shift-Tab` | cycle focus (Local → Remote → Transfers → Log; the last two are tabs of the bottom pane) |
| `↑` / `↓`      | move cursor; in the Log pane, scroll the log (`PgUp` / `PgDn` a page) |
| `↵`            | enter directory (or a remote symlink to one) |
| `backspace`    | go up to parent directory                    |
| `space`        | select / deselect                            |
| `Ctrl-D`       | download selected items                      |
| `Ctrl-U`       | upload selected items                        |
| `v`            | view image or text                           |
| `/`            | filter current pane                          |
| `F5`           | refresh active pane                          |
| `F2`           | rename (remote pane)                         |
| `F7`           | create new remote directory                  |
| `Shift-Del` / `D` | delete file or folder (remote pane)       |
| `Ctrl-S`       | save current session                         |
| `Ctrl-X`       | disconnect (return to selector, with confirmation) |
| `t`            | cycle theme                                  |
| `c`            | cancel selected transfer (Transfers pane)    |
| `C`            | cancel whole batch (Transfers pane)          |
| `r`            | resume interrupted download batch (Transfers pane) |
| `R`            | resume interrupted upload batch (Transfers pane)   |
| `p`            | pause / resume all transfers                 |
| `?`            | toggle help                                  |
| `q` / `esc`    | quit (confirms unless `confirm_quit = false`) |
| `Ctrl-C`       | quit from any screen, through the same prompt; again on the prompt to quit at once |

In the session selector: `n` new, `e` edit, `d` delete, `t` cycle theme.

In the viewer: `↑` / `↓` or `k` / `j` scroll a line, `PgUp` / `PgDn` (or
`space`) scroll a page, `Home` / `g` and `End` / `G` jump to the top and
bottom, and `q` / `esc` close it.

### Mouse

On by default; `mouse = false` under `[terminal]` in `config.ini` turns it
off and leaves the terminal's own text selection alone.

- **Click** a file pane to focus it; click a row to move the cursor there.
- **Double-click** a directory (or `..`, or a remote symlink to a
  directory) to enter it; a remote file to download it, or a local file to
  upload it — just that file, whatever else is selected.
- **Scroll wheel** moves the cursor in the pane under the mouse, the
  transfer list, and the session list, and scrolls the log and the viewer.
- **Click** the TRANSFERS / LOG tabs to switch them, and a transfer to select
  it; a click anywhere else in the bottom pane focuses it, and so does the
  wheel over the transfer list or the log. In the session selector, click a session to
  select it and double-click to connect.

Prompts and modals ignore the mouse, and so do clicks with Ctrl or Alt held.
With capture on, most terminals need Shift held to select text.

## Supported viewer formats

`v` opens the in-app viewer for the cursor item. Whether a file is recognised
depends on its extension (or, for a few well-known names, the bare filename).
False negatives just mean you have to download to read it; nothing is
guessed by content sniffing.

### Images (any of the three supported terminal graphics protocols)

`.png`, `.jpg` / `.jpeg`, `.gif`, `.webp`

Display caps at 25 MB. That bounds the fetch, not the decode, which has
limits of its own; see [Resource limits](#resource-limits).

### Text

Display caps at 1 MB.

**Bare filenames recognised without an extension:**
`README`, `LICENSE` / `LICENCE`, `Makefile`, `GNUmakefile`, `Dockerfile`,
`justfile`, `CHANGELOG`, `AUTHORS`, `CONTRIBUTORS`, `TODO`, `NOTICE`, and the
shell dotfiles `.bashrc`, `.zshrc`, `.profile`, `.bash_profile`,
`.bash_aliases`

**Recognised extensions, by category:**

| Category    | Extensions                                                    |
| ----------- | ------------------------------------------------------------- |
| Generic     | `txt`, `md`, `rst`, `log`                                     |
| Config      | `ini`, `conf`, `cfg`, `config`, `env`, `gitignore`, `gitattributes`, `editorconfig` |
| Data        | `json`, `jsonc`, `yaml`, `yml`, `toml`, `xml`, `csv`, `tsv`   |
| Web         | `html`, `htm`, `css`, `scss`, `sass`, `less`                  |
| JavaScript  | `js`, `mjs`, `cjs`, `ts`, `jsx`, `tsx`                        |
| Systems     | `rs`, `c`, `h`, `cpp`, `cxx`, `cc`, `hpp`, `go`               |
| Scripting   | `py`, `pyw`, `pyi`, `rb`, `lua`, `pl`, `r`, `php`             |
| JVM/.NET    | `java`, `kt`, `swift`, `cs`                                   |
| Shell       | `sh`, `bash`, `zsh`, `fish`, `ps1`, `bat`                     |
| Database    | `sql`                                                         |
| Patches     | `diff`, `patch`                                               |
| Release     | `nfo`                                                         |

Text decoding: NFO files are decoded as CP437 (DOS codepage 437), preserving
box-drawing characters — unless they are valid UTF-8, as the `.nfo` files
media managers such as Kodi write are, in which case they are read as UTF-8.
All other text files are decoded as UTF-8 (lossy —
unrecognised byte sequences render as replacement characters rather than
failing the viewer).

If you'd like another extension recognised, the allowlist is one match arm
in `src/preview.rs::is_viewable_text`. Anything the syntax highlighter has a
language for (`src/highlight.rs::lang_for_name`) is recognised too.

## Architecture

```
src/
├── main.rs              entrypoint, CLI parsing, terminal lifecycle
├── error.rs             one error enum to rule them all
├── paths.rs             platform config / session / theme / checkpoint dirs; sync_parent_dir helper
├── config.rs            global config.ini load / save (preserves unknown keys)
├── session.rs           per-session .ini load / save / list / URL parser
├── theme.rs             theme model + 7 built-ins + file loader
├── checkpoint.rs        walk-plan checkpointing: persist, append, remove batch state; debounced fsync writes; version-4 format, origin-keyed
├── known_hosts.rs       SSH host-key store: check / append / remove, OpenSSH-style matching, file lock, SessionTrust (per-session "trust once")
├── highlight.rs         syntax highlighter for the text viewer (single-pass, zero deps)
├── transport/           connection layer
│   ├── mod.rs           Transport trait + factory + Connected struct + part_path / part_meta_path + resume provenance (decide_resume)
│   ├── sftp.rs          SFTP via russh + russh-sftp (SSH keepalive, rsa-sha2-512)
│   ├── sftp_test_keys.rs  test-only: published throwaway host/client keys for the in-process SFTP server — not secrets
│   ├── scp.rs           transparent SFTP wrapper (matches OpenSSH 9.0+); delegates via the delegate_inner_transport! macro
│   ├── ftp.rs           FTP via suppaftp tokio backend
│   ├── ftps.rs          FTPS via suppaftp + rustls; pinning verifier (hostname + signature + cert pin)
│   ├── ftp_impl.rs      shared macro that generates the Transport impl for FTP and FTPS; per-call deadline, reconnect after a broken call
│   └── error_map.rs     maps russh-sftp / suppaftp errors to typed BlinkError variants (NotFound / Permission / Disconnected)
├── transfer.rs          TransferManager: queue, state, progress events; MAX_QUEUED_JOBS cap
├── transfer/
│   └── dispatcher.rs    parallel slot dispatcher; per-job worker tasks; zeroized password
├── preview.rs           kitty / sixel / iterm2 backends; FileViewKind classification; CP437 NFO decoder
└── tui/
    ├── mod.rs           terminal init / restore + top-level run wrappers
    ├── event.rs         keyboard / app-event multiplexer
    ├── hit.rs           where clickable things were last drawn, for matching mouse events
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
        ├── mouse.rs     mouse handling: focus, clicks, double-clicks, wheel
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
  only `Enter` / `Esc` / `q` dismisses. A known host presenting only a
  keytype that isn't on file counts as changed too, so a man in the middle
  cannot pass a downgrade off as a new host. The matching rules, the
  prompt and how to forget a host are under
  [`known_hosts`](#known_hosts--ssh-host-key-store).
- **SSH host certificates are refused, fail-closed.** `known_hosts` maps a
  host to a literal key and blink has no `@cert-authority` support, so
  nothing here can check a certificate's CA signature, principals, or
  validity window. Pinning one by its key would look like verification
  while checking none of that, so a certificate is rejected outright and
  the reason is surfaced in the log rather than left as a generic connect
  failure.
- **SFTP RSA auth uses rsa-sha2-512.** ssh-rsa with SHA-1 has been disabled
  by default in OpenSSH 8.8+ (September 2021); blink negotiates the
  modern hash so RSA users don't get an opaque "rejected by server"
  against any current OpenSSH.
- **SSH keepalive at 30 s × 3.** An authenticated session with a dead TCP
  underneath tears down in ~90 s instead of waiting on the OS keepalive
  (which can run into minutes).
- **FTPS** — explicit TLS only (RFC 4217), verified against the Mozilla CA
  bundle via rustls (pure Rust, no system OpenSSL).
- **Plain FTP is unencrypted, and says so.** `USER` / `PASS` and every byte
  of every transfer cross the network in the clear, and an observer on the
  path can read or alter them. There is no protocol-level fix for this —
  it is what `ftp://` is — so blink does the one useful thing instead:
  a warning in the log on every connect, and the protocol tag coloured as a
  caution in the session selector. FTPS, being encrypted, is not flagged.
- **FTP data connections go to the server, wherever PASV points.** A
  `227` reply names an address as well as a port, and the server chooses
  it — so a hostile one could aim the data connection at any host, such as
  a service on your own network that an upload would then write to. blink
  dials the control connection's peer on the reply's port instead, as curl
  does by default. Applies to FTP and FTPS alike.
- **One crypto backend, not two.** `ring` serves both transports: rustls uses
  it for FTPS, and `russh` is built against it rather than its default
  `aws-lc-rs`. With both compiled in, rustls refuses to choose between them
  and every FTPS connect panics, so the manifest pins one and a test fails
  if the other returns through feature unification.
- **FTPS hostname + signature verification are mandatory.** Trust anchors
  are the embedded Mozilla bundle (`webpki-roots`), not the system store,
  so self-signed and privately-rooted certificates aren't trusted by
  default. Even when the per-session
  `accept_invalid_certs = true` bypasses CA-chain trust, the cert's SAN
  must still match the configured hostname and the handshake signature
  must still verify against the cert's public key. The flag also enables
  TOFU pinning: on the first connect blink shows the leaf cert's SHA-256
  (as `openssl x509 -fingerprint -sha256` prints it) and asks, before
  sending the password, whether to trust it. Accepted, it's recorded in the
  session, and subsequent connects require an exact match. The data
  connections are held to the same certificate, during the first connect
  too.
- **30-second connect timeout** — applied to both the primary connection
  and every parallel worker connection. A server that accepts the TCP
  socket but stalls the handshake cannot pin connections indefinitely.
  Time spent with the host-key prompt open does not count against it, so
  checking a fingerprint out of band cannot time the connection out. The
  prompt has its own limit of two minutes, OpenSSH's default
  `LoginGraceTime`, after which the server would drop the connection
  anyway.
- **An FTP/FTPS connection that breaks is replaced, not reused.** Each
  control-channel call has a 60-second deadline. A call that passes it,
  is cut short, or loses the connection leaves the client possibly out
  of step with the server, so the next call reconnects and logs in
  afresh before it runs. An FTPS reconnect must present the certificate
  the first connect accepted.

### Terminal injection prevention

All server-controlled strings pass through a sanitizer before being stored or
rendered. Two classes of character are replaced with spaces:

- **Control characters** (U+0000–U+001F and U+007F–U+009F) — covers ESC and
  every ANSI sequence starter.
- **Bidirectional, zero-width and other invisible characters**: the soft
  hyphen (U+00AD), U+061C, the Hangul fillers (U+115F–U+1160, U+3164,
  U+FFA0), U+180E, U+200B–U+200C, U+200E–U+200F, U+202A–U+202E, the word
  joiner and invisible operators (U+2060–U+2064), U+2066–U+2069, the
  deprecated formatting characters (U+206A–U+206F), U+FEFF, the
  interlinear annotation characters (U+FFF9–U+FFFB), the musical
  formatting characters (U+1D173–U+1D17A) and the tag characters
  (U+E0000–U+E007F). None of these are control characters, so an
  `is_control()` check alone misses them. A RIGHT-TO-LEFT OVERRIDE lets a
  server name a file that *renders* as `report.png` while the bytes end in
  `.exe`; invisible characters let two different names render identically.
  Replacing rather than deleting is deliberate — it makes the difference
  visible instead of collapsing two distinct names onto one rendering.
  U+200D ZERO WIDTH JOINER and the variation selectors are intentionally
  kept: they can't spoof an extension, and stripping them would break
  emoji sequences in legitimate filenames.

Applied to:

- Remote directory-entry names (`list()` in every transport)
- SSH key-type strings and host-key fingerprints
- Error messages from transport layers
- Text file content in the viewer (tabs expanded to 8-column stops) — the
  bidi filter
  matters as much here, since viewing remote source is exactly the
  "trojan source" setting
- Session and checkpoint names printed by the CLI subcommands

### Path safety

- **Remote-to-local path traversal** — entry names containing `/`, `\`, `\0`,
  or equal to `..` / `.` are rejected before `Path::join` in download paths
  and recursive walks (`safe_local_name()`). Windows additionally rejects
  `:` (a drive prefix like `C:evil` would otherwise *replace* the
  destination path per `PathBuf::push` semantics, and `name:stream` opens
  an alternate data stream), trailing dots and spaces (Windows strips
  them, aliasing two names to one file), and reserved device names (`NUL`,
  `COM1`, …).
- **Remote path injection** — `join_remote()` strips leading `/` from
  server-supplied names and rejects any `..` component, preventing a server
  from escaping the working directory via path construction.
- **Recursive walks skip symlinks.** A server-side symlink
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
  A recursive `delete_dir(true)` that recursed through a symlink to a
  directory would delete files outside the chosen subtree — possibly
  outside the connection's chroot. Only a real directory is recursed into;
  symlinks, sockets and devices are unlinked as leaves. Entries are
  classified by comparing the whole file-type field of their mode, not by
  testing individual bits: the POSIX type codes overlap, so a bit test
  reads a socket or block device as a directory.
- **Password-in-URL is rejected.** `sftp://alice:hunter2@host/` errors
  with a pointer at the interactive prompt rather than smuggling the
  password into the username field and into shell history.

### Resource limits

| Resource | Limit |
| -------- | ----- |
| Text file preview read | 1 MB |
| Image file preview read | 25 MB |
| Image decoder pre-decode dimension cap | 4096 × 4096 px |
| Image decoder max allocation | 128 MiB (enforced *before* full pixel-buffer alloc) |
| Transfer job queue | 100,000 jobs |
| Recursive walk plan | 100,000 entries — planned jobs *plus* directories visited and queued (bails before materialising the whole tree) |
| Error string length | 512 characters |
| Session / config / theme files | 64 KiB each |
| Checkpoint files | 256 MiB |
| Known-hosts file | 1 MiB |

The preview limits are checked against the size the listing reports and
again against the bytes that actually arrive, so a server that lies in its
directory listing cannot bypass them. The image decoder limits are set via
`image::Limits`, so the decoder refuses oversized declarations on the
header rather than after the full RGBA buffer is allocated — a small file
declaring huge dimensions is still rejected.

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
- Each parallel worker slot opens its own authenticated connection, which
  is why the credential has to outlive the first login. The workers share
  one zeroized copy by reference rather than each holding their own; an
  FTP/FTPS connection also keeps one to reconnect with after it breaks.
- If holding a password in memory doesn't fit your threat model, use SSH
  key auth or ssh-agent instead — the key file (or the agent's identity
  store) stays put and no in-memory copy of the secret is needed.

### Config and session file safety

- **A session is validated before it is written.** `Session::save` refuses
  anything `load_from` would reject — a relative `local_dir`, a relative
  `key_path`, a field carrying a newline — so no form can save a file the
  loader would then skip, leaving a session on disk that the selector
  can't show.
- **Session files that fail to load are reported, not silently dropped.**
  `blink sessions` prints a `warning: skipped <file>: <reason>` line to
  stderr, and the TUI logs one per unreadable file at startup and on every
  reload.
- Session, config, and checkpoint files are written with the full
  atomic-and-durable pattern: tempfile → `sync_all` → rename →
  `sync_all` on the parent directory. Without the syncs, a power loss
  between rename and the filesystem journal commit can leave a
  zero-byte file or roll the rename back. (Unix only; on Windows the
  filesystem journals rename through its own ordering rules.)
- Checkpoint writes during a hot transfer batch are debounced — at least
  250 ms apart, and further apart for a large checkpoint, so that writing
  takes no more than about a twentieth of the time — so a 100k-job batch
  doesn't generate ~200k full plan rewrites. Any lost mark on a crash just
  causes the affected job to be re-queued on resume — never silently
  skipped.
- Downloads write to a `<local>.<hash>.blink-part` sibling and rename onto the
  final name only after `flush` + `sync_all`. The user's existing file (if
  any) isn't truncated until the new download has fsynced cleanly. The
  partial's name and the `.meta` sidecar that gates resume are described
  under [Transfers](#transfers).
- Uploads mirror this on the remote side: bytes stream into
  `<remote>.<hash>.blink-part` and the final name is only created by rename after
  the upload completes (and fsyncs, where the server supports
  `fsync@openssh.com`). SFTP uses `posix-rename@openssh.com` for an
  atomic replace when the server offers it; otherwise (and on FTP/FTPS,
  where overwrite-on-rename is server-dependent) a file in the way is
  moved aside to `<remote>.<hash>.blink-old`, the rename retried, and the
  old file removed once it lands, or moved back if it doesn't — that
  window can expose "old file at `.blink-old`, new file still at
  `.blink-part`", but never a truncated file under the final name. An
  upload interrupted by a hard kill or dropped connection
  can leave a stale `<remote>.<hash>.blink-part` behind; re-running the upload
  reuses (truncates) it.
- Config directories are created with mode 0700 on Unix (not
  world-readable). A log file gets mode 0600; see [Logging](#logging).
- `Config::save` round-trips unknown INI keys, so hand-edited or
  forward-compat options aren't silently dropped on a theme cycle.
- Path-traversal and null-byte validation is applied when loading
  session and theme names from disk.

## Honest caveats

A few things worth knowing before you use this in anger:

- **FTPS is explicit-only.** The `AUTH TLS` upgrade path (RFC 4217) is what
  every modern server speaks. Implicit FTPS on the deprecated port 990 is
  not supported. If you have a server that only does implicit-mode, you'd
  need a different connect path; the `transport/ftps.rs` seam is small.
- **FTP directory listings are parsed as POSIX (`ls -l`) or DOS, nothing
  else.** blink issues `LIST` and never `MLSD`/`MLST`, so it parses with
  those two parsers only. Lines in neither format are skipped, and the log
  pane says how many ("3 of 40 entries in /pub couldn't be read"), or that
  none could be, when the directory comes up empty for that reason. A
  recursive download that skipped entries this way says how many, so the
  batch isn't mistaken for complete. The library's fallback parser, which
  accepts *any* line and names the file after it, is deliberately not
  used: it would put entries in the pane that address nothing.
  Real-world dialect coverage is untested: the FTP test suite runs against
  an in-process server, not vsftpd or IIS.
- **No custom CA roots for FTPS.** Trust anchors are the embedded Mozilla
  bundle, and there is no option to add a root without recompiling. For a
  self-signed or privately-rooted server, the per-session
  `accept_invalid_certs` toggle pins the certificate instead (see
  [Security](#protocol)).
- **Passwords are held in memory** for the duration of the connected
  session; see [Credential handling](#credential-handling) for why, and
  for the alternative.
- **No partial-tree cancel.** `c` cancels one transfer and `C` the whole
  batch; there is no way to cancel only a subdirectory within a larger
  walk.
- **Walk checkpoints survive a clean exit *and* most hard kills.** The
  initial plan is written with `flush()` (synchronous) before any job
  runs. Per-job state changes are debounced (see
  [Config and session file safety](#config-and-session-file-safety)) — a
  crash within that window leaves the affected job in its previous state
  on resume, which is the same safe outcome as a crash mid-transfer
  (Pending → re-queued, InProgress → re-queued, Done → re-run). `mkdir` is
  idempotent on the remote side, so re-runs are safe across the board.
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
| [tokio-util](https://github.com/tokio-rs/tokio) | Tokio contributors | Async I/O utilities (reached through `russh-sftp`) |
| [tracing](https://github.com/tokio-rs/tracing) | Tokio contributors | Structured logging |
| [tracing-subscriber](https://github.com/tokio-rs/tracing) | Tokio contributors | Log sink / filter |
| [bytes](https://github.com/tokio-rs/bytes) | Tokio contributors | Byte buffer utilities |
| [rust-ini](https://github.com/zonyitoo/rust-ini) | Y. T. | INI config parser |

### MIT OR Apache-2.0

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [async-trait](https://github.com/dtolnay/async-trait) | David Tolnay | Async trait support |
| [futures](https://github.com/rust-lang/futures-rs) | Alex Crichton et al. | Future combinators |
| [suppaftp](https://github.com/veeso/suppaftp) | Christian Visintin | FTP / FTPS client |
| [tokio-rustls](https://github.com/rustls/tokio-rustls) | rustls contributors | Async TLS via rustls (reached through `suppaftp`, which re-exports it) |
| [serde](https://github.com/serde-rs/serde) | David Tolnay, Erick Tryzelaar | Serialisation framework |
| [serde_json](https://github.com/serde-rs/json) | David Tolnay, Erick Tryzelaar | JSON serialisation (checkpoints) |
| [directories](https://github.com/dirs-dev/directories-rs) | Simon Ochsenreither | Config-dir path on platforms other than Linux, macOS and Windows (those three are resolved directly) |
| [clap](https://github.com/clap-rs/clap) | clap contributors | CLI argument parsing |
| [thiserror](https://github.com/dtolnay/thiserror) | David Tolnay | Error derive macro |
| [image](https://github.com/image-rs/image) | image-rs contributors | Image decoding (PNG, JPEG, GIF, WebP) |
| [base64](https://github.com/marshallpierce/rust-base64) | Marshall Pierce et al. | Base64 encoding for image preview |
| [chrono](https://github.com/chronotope/chrono) | chronotope contributors | Date / time formatting |
| [sha2](https://github.com/RustCrypto/hashes) | RustCrypto contributors | SHA-256 for FTPS cert pins, partial-file names and session file names |
| [zeroize](https://github.com/RustCrypto/utils/tree/master/zeroize) | RustCrypto contributors | Wipe in-memory passwords on drop |
| [icy_sixel](https://github.com/mkrueger/icy_sixel) | Mike Krüger | Sixel image encoding |
| [parking_lot](https://github.com/Amanieu/parking_lot) | Amanieu d'Antras | Faster synchronisation primitives |
| [libc](https://github.com/rust-lang/libc) | The Rust Project Developers | `O_NOFOLLOW` for opening partial downloads and their sidecars (Unix only) |

### Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [rustix](https://github.com/bytecodealliance/rustix) | Bytecode Alliance contributors | Safe `geteuid`, to tell your own symlinks from another user's (Unix only) |

### Apache-2.0

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [russh](https://github.com/warp-tech/russh) | Eugeny, Pierre-Étienne Meunier | SSH transport (SFTP / SCP) |
| [russh-sftp](https://github.com/AspectUnk/russh-sftp) | AspectUnk | SFTP protocol layer |

### Community Data License Agreement – Permissive 2.0 (CDLA-Permissive-2.0)

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [webpki-roots](https://github.com/rustls/webpki-roots) | Mozilla / rustls contributors | Mozilla CA root certificates for FTPS |

### Apache-2.0 AND ISC

| Crate | Author(s) | Use in blink |
| ----- | --------- | ------------ |
| [ring](https://github.com/briansmith/ring) | Brian Smith et al. | Crypto backend for both SSH and TLS |
