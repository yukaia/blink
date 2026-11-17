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

From `cargo llvm-cov` on 2026-11-17, after a pass over the previous list
(now 93% of lines, 92% of functions). Re-run it before picking one up;
`cargo llvm-cov --json` lists functions with a count of zero. In rough
order of value:

- **App events.** `tui/app/events.rs` is at 68%. Unrun arms of
  `handle_app_event`: most of `Connected` (choosing the local folder,
  queuing post-connect offers), `ConnectKeyNeedsPassphrase` after a
  wrong passphrase, `LocalListFailed`, `Renamed`, `WalkComplete` and
  `WalkFailed`, and `ViewLoaded`'s NFO and over-limit paths.
- **Starting transfers.** `tui/app/transfers.rs`: `start_upload_walk`,
  `start_download_walk`, and `confirm_overwrite_proceed`'s skip path,
  driven from the app rather than the walk functions underneath.
- **Things that need a real terminal.** `App::run`, `tui::run` and
  `run_with_session`, raw mode, the panic hook's UI-thread check,
  `viewer::after_draw` (writes to stdout) and `main`'s dispatch. The
  escape sequences for entering and leaving the screen are covered;
  the rest would need a pseudo-terminal harness.
- **Dead code.** `handle_new_session`'s key/agent arm can't run:
  `Session::from_url` always gives password auth.
