# Backlog

Things worth doing that nothing is currently blocked on. Each entry should say
enough to restart cold, and no more — if an item grows past that, it wants a
spec in `docs/superpowers/specs/` instead.

---

## `ssh-key` and `rsa` reach a release

Watch `russh` rather than `ssh-key` and `rsa` directly: it pins both with exact
`=` requirements, so they move when it moves. Both are still pre-GA
(`ssh-key 0.7.0-rc.11`, `rsa 0.10.0-rc.18`).

A release alone does not retire the RUSTSEC-2023-0071 ignore in
`.cargo/audit.toml`: the advisory lists no patched version, and names
rc.18 as affected. The fixes are RustCrypto/RSA#739 (blinding the default
decryption paths) and #749 (implicit rejection), which replaced #702 and
#680 and closed #626. Both merged 2026-11-01, and `rsa 0.10.0-rc.20`
(published 2026-11-03) should carry them — confirm against its changelog.
What's left is russh moving its pin to rc.20 or later; then check whether
RUSTSEC-2023-0071 names a patched version before dropping the ignore.

Last checked 2026-11-13: `rsa 0.10.0` is out as a full release and
`ssh-key` is at `0.7.0-rc.12`, but russh 0.65.1, the latest and what blink
is on, still pins `rsa =0.10.0-rc.18` and `ssh-key =0.7.0-rc.11`.

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
