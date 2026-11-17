# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `ssh-key` reaches a release

Watch `russh` rather than `ssh-key` directly: it pins it with an exact `=`
requirement, so it moves when russh moves. blink is on a pre-release
(`ssh-key 0.7.0-rc.12`).

`rsa` is done: russh 0.65.2 relaxed its pin to `rsa 0.10`, the lockfile is on
`rsa 0.10.0`, and the RUSTSEC-2023-0071 ignore and README caveat are gone.

Last checked 2026-11-17: russh 0.65.2, the latest and what blink is on, pins
`ssh-key =0.7.0-rc.12`; no `ssh-key 0.7.0` release yet.

## russh can't load an encrypted legacy-PEM ECDSA key

Found while pinning the passphrase errors (`key_needs_passphrase` in
`sftp.rs`): an ECDSA key written by `ssh-keygen -t ecdsa -m PEM` with a
passphrase (`BEGIN EC PRIVATE KEY`, `Proc-Type: 4,ENCRYPTED`) fails to
load even with the right passphrase. russh 0.64.1 decrypts it, then
parses the result as PKCS#1 and reports "unexpected ASN.1 DER tag:
expected INTEGER, got OCTET STRING". The user sees a load error, not a
passphrase prompt. The same key in OpenSSH or PKCS#8 format loads fine,
and `ssh-keygen -p -f <key>` converts it to OpenSSH. The unencrypted
form of the same key loads. Upstream's bug, still there in russh 0.65.1
(checked 2026-11-13) and not yet reported. There's nothing for blink to do
beyond, at most, naming the workaround in the error.

## Features with no tests

From `cargo llvm-cov` runs on 2026-11-17 (84% of lines, 88% of functions
after the tests added that day). Re-run it before picking one up; `cargo llvm-cov --json` lists
functions with a count of zero. In rough order of value:

- **Host-key rejections in the TUI.** The `HostKeyChanged`,
  `HostKeyRevoked` and `HostCertificateRejected` arms of
  `handle_app_event`: what the user is told is unchecked.
- **SFTP's own mkdir, delete_file and rename.** The TUI's mkdir, delete
  and rename actions are tested against the mock transport;
  `SftpTransport::mkdir`, `delete_file` and `rename` are never called
  against the test server.
- **The rest of the CLI.** `main.rs` has no tests: `sessions`, `themes`,
  `known-hosts remove`'s output, and `init_tracing` with its check of the
  log file's mode. (`known_hosts::remove_host` and
  `checkpoint::list_and_clean` themselves are covered.)
- **Rendering.** Nineteen of the `render` functions in `tui/views.rs`
  never run: every modal but the host-key ones, and help. One smoke test
  each against ratatui's `TestBackend` would do.
- **Key handling.** `handle_main` (69 of 122 lines unrun) and the viewer,
  save-session, quit and disconnect handlers.
- **Image preview.** The Kitty and iTerm2 backends and `decode_cp437`;
  only Sixel runs. `after_draw`, `image_area`, `viewer_scroll_to`.
- **SCP.** `transport/scp.rs` is a wrapper over SFTP that no test
  connects through.
- **The folder guard in the upload rename fallback.** A folder in the way
  of an upload is left alone (`finalize_remote_rename_unbounded`,
  `ftp_upload`), but neither test server can rename a folder, so nothing
  exercises it.
- **Terminal lifecycle.** `tui/mod.rs`: setup, restore, the panic hook.
- **Small helpers.** `format_bytes_per_sec`, `format_eta`,
  `resolve_local_dir`, `Theme::list_all_names`, `Config::load`, and the
  edit form's field navigation in `tui/state.rs`.
