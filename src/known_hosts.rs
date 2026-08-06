//! Known-hosts store: read, check, and append host keys.
//!
//! The file lives at `~/.config/blink/known_hosts` and uses the same line
//! format as OpenSSH's `~/.ssh/known_hosts`:
//!
//! ```text
//! hostname key-type base64-public-key
//! ```
//!
//! Lines beginning with `#` are comments and are preserved on rewrite.
//!
//! ## Hostname forms
//!
//! Following OpenSSH conventions, blink stores entries as:
//!
//! - `host` when the port is the SSH default (22),
//! - `[host]:port` when the port is non-default.
//!
//! Hostnames are lowercased on both store and lookup. For backward compat
//! with older blink versions that wrote `host:port` unconditionally, lookups
//! also accept the legacy `host:port` form (case-insensitive).
//!
//! ## Unsupported forms
//!
//! Hashed entries (`|1|salt|hash`) written by OpenSSH with
//! `HashKnownHosts=yes` are **not** recognised. Blink writes its own file
//! and only looks up entries it wrote itself; importing from
//! `~/.ssh/known_hosts` is out of scope.
//!
//! ## Match semantics
//!
//! Lookup mirrors OpenSSH's `(host, keytype)` matching:
//!
//! - [`KeyStatus::Trusted`] — some matching line has the exact `(host,
//!   keytype, key)` triple.
//! - [`KeyStatus::Changed`] — some matching line has the same `(host,
//!   keytype)` but a different key. Hard error (possible MITM).
//! - [`KeyStatus::Unknown`] — no matching line for the host, or only
//!   matching lines for *different* keytypes. Ask the user.
//!
//! In particular, a host with both `ssh-ed25519` and `ssh-rsa` entries does
//! not flag `Changed` when only one of them is presented — that's normal
//! multi-algorithm behaviour.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{self, BlinkError, Result};
use crate::paths;

/// Maximum size of the known_hosts file accepted on load (1 MiB).
const MAX_KNOWN_HOSTS_BYTES: u64 = 1024 * 1024;

/// Default SSH port — entries for this port are stored without `[host]:port`
/// brackets, matching OpenSSH.
const DEFAULT_SSH_PORT: u16 = 22;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of checking a host key against the known-hosts file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    /// Host + key pair is in the file. Proceed.
    Trusted,
    /// Host is not in the file. Ask the user.
    Unknown,
    /// Host is in the file with the same key-type but a different key.
    /// Hard reject.
    Changed {
        /// The key type stored in the file (e.g. `ssh-ed25519`).
        stored_key_type: String,
        /// The base64 key stored in the file.
        stored_key_b64: String,
    },
}

// ---------------------------------------------------------------------------
// File path
// ---------------------------------------------------------------------------

pub fn known_hosts_path() -> Result<PathBuf> {
    Ok(paths::root_dir()?.join("known_hosts"))
}

// ---------------------------------------------------------------------------
// Hostname formatting
// ---------------------------------------------------------------------------

/// Canonical form used when storing a new entry.
///
/// - Bare lowercased host for the default SSH port.
/// - `[host]:port` (lowercased) otherwise.
fn canonical_host(host: &str, port: u16) -> String {
    let h = host.to_ascii_lowercase();
    if port == DEFAULT_SSH_PORT {
        h
    } else {
        format!("[{h}]:{port}")
    }
}

/// Legacy form previously written by blink: always `host:port`,
/// case-preserved. We accept this on lookup for backward compatibility.
fn legacy_host(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Whether `file_host` (a known_hosts hostname field) refers to
/// `(host, port)`. Accepts the canonical form and the legacy
/// `host:port` form (case-insensitive).
fn host_matches(file_host: &str, host: &str, port: u16) -> bool {
    let canonical = canonical_host(host, port);
    if file_host.eq_ignore_ascii_case(&canonical) {
        return true;
    }
    let legacy = legacy_host(host, port);
    file_host.eq_ignore_ascii_case(&legacy)
}

// ---------------------------------------------------------------------------
// Core operations
// ---------------------------------------------------------------------------

/// Check whether `(host, port, key_type, key_b64)` is in the known-hosts file.
pub fn check(host: &str, port: u16, key_type: &str, key_b64: &str) -> Result<KeyStatus> {
    let path = known_hosts_path()?;
    let raw = match read_bounded(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(KeyStatus::Unknown),
        Err(e) => return Err(BlinkError::from(e)),
    };
    Ok(check_in_str(&raw, host, port, key_type, key_b64))
}

fn check_in_str(raw: &str, host: &str, port: u16, key_type: &str, key_b64: &str) -> KeyStatus {
    // OpenSSH semantics: keep scanning all matching lines.
    // - Any line whose (host, key_type, key_b64) all match → Trusted.
    // - Else if any line with this (host, key_type) has a different blob →
    //   remember it as a potential Changed result.
    // - Else → Unknown.
    let mut changed: Option<KeyStatus> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let (file_host, file_type, file_b64) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(t), Some(k)) => (h, t, k.trim()),
            _ => continue, // malformed line — skip
        };

        if !host_matches(file_host, host, port) {
            continue;
        }
        if file_type != key_type {
            // Different algorithm for the same host is normal; ignore.
            continue;
        }
        if file_b64 == key_b64 {
            return KeyStatus::Trusted;
        }
        // Same host, same algorithm, different blob — remember as Changed
        // (but keep scanning in case a later line is a Trusted match).
        if changed.is_none() {
            changed = Some(KeyStatus::Changed {
                stored_key_type: error::sanitize(file_type.to_string()),
                stored_key_b64: error::sanitize(file_b64.to_string()),
            });
        }
    }

    changed.unwrap_or(KeyStatus::Unknown)
}

/// Append a new entry to the known-hosts file.
///
/// Creates the file if it does not exist. Takes an exclusive advisory lock
/// across the check-then-write so two concurrent blink processes accepting
/// the same host cannot interleave or duplicate the line. Writes the entry
/// in the canonical `[host]:port` form (or bare host for port 22).
pub fn append(host: &str, port: u16, key_type: &str, key_b64: &str) -> Result<()> {
    let stored_host = canonical_host(host, port);

    // Reject characters that would corrupt the whitespace-delimited format or
    // allow a malicious server to inject trusted entries.
    for (field, value) in [("host", host), ("key_type", key_type), ("key_b64", key_b64)] {
        if value.bytes().any(|b| matches!(b, b'\n' | b'\r' | b'\0')) {
            return Err(BlinkError::config(format!(
                "invalid control character in known_hosts field '{field}'"
            )));
        }
    }
    // Spaces in the host or key_type would silently break the 3-field format
    // when the line is re-parsed, potentially aliasing one entry to another.
    if host.contains(' ') {
        return Err(BlinkError::config(
            "space not allowed in known_hosts field 'host'",
        ));
    }
    if key_type.contains(' ') {
        return Err(BlinkError::config(
            "space not allowed in known_hosts field 'key_type'",
        ));
    }

    let path = known_hosts_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Open read+write, creating if missing. We hold the same handle for the
    // duration of the check + append so the advisory lock covers both.
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)?;

    // Take an exclusive advisory lock to make the check-then-write sequence
    // atomic with respect to other blink processes. Blocks until acquired —
    // acceptable here because the held region is small. `File::lock` is std's
    // (stable since Rust 1.89) and releases on drop, same flock semantics the
    // fs4 crate provided before.
    file.lock()
        .map_err(|e| BlinkError::config(format!("known_hosts lock: {e}")))?;

    let raw = read_bounded_from_handle(&mut file)?;
    if matches!(
        check_in_str(&raw, host, port, key_type, key_b64),
        KeyStatus::Trusted
    ) {
        // Lock released on drop.
        return Ok(());
    }

    // Ensure we write at the end even though `append(true)` should guarantee
    // it; on some platforms the read above moved the cursor.
    file.seek(SeekFrom::End(0))?;
    writeln!(file, "{stored_host} {key_type} {key_b64}")?;
    // Lock released on drop.
    Ok(())
}

/// Remove every entry for `(host, port)` from the known-hosts file.
///
/// Returns how many lines were removed, so the caller can tell "forgot the
/// key" apart from "nothing matched" — the latter usually means the user
/// named a different host form than the one that was stored (a bare host
/// when the entry is `[host]:port`, or vice versa).
///
/// Matching accepts the same forms as [`check`]: the canonical bare host for
/// port 22, the bracketed `[host]:port`, and the legacy `host:port`.
pub fn remove_host(host: &str, port: u16) -> Result<usize> {
    let path = known_hosts_path()?;

    let raw = match read_bounded(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(BlinkError::from(e)),
    };

    let (filtered, removed) = filter_out_host(&raw, host, port);

    // Nothing to do — don't rewrite the file (and don't risk clobbering a
    // concurrent append) just to produce identical content.
    if removed == 0 {
        return Ok(0);
    }

    // Atomic + durable write, same pattern as every other file blink owns:
    // tempfile → sync_all → rename → fsync the parent directory. Without the
    // syncs a power loss can leave a zero-byte known_hosts, which would
    // silently downgrade every stored host to "unknown".
    //
    // Note this does NOT take the advisory lock `append` uses: that lock
    // lives on the original inode, and renaming a replacement over it can't
    // be serialised against it that way. A concurrent accept-and-save racing
    // this removal can therefore be lost — the consequence is one re-prompt
    // on the next connect, not a wrong trust decision.
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(filtered.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    paths::sync_parent_dir(&path)?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Drop every entry matching `(host, port)` from `raw`, returning the
/// rewritten contents and how many lines went away.
///
/// Comments and blank lines are preserved, and lines whose host field names a
/// different host or a different port are left alone — removing a key must
/// not disturb neighbouring entries.
fn filter_out_host(raw: &str, host: &str, port: u16) -> (String, usize) {
    let mut removed = 0usize;
    let filtered: String = raw
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                return true; // keep comments and blanks
            }
            let host_field = t.split(' ').next().unwrap_or("");
            let matched = host_matches(host_field, host, port);
            if matched {
                removed += 1;
            }
            !matched
        })
        .map(|l| format!("{l}\n"))
        .collect();
    (filtered, removed)
}

/// Open `path` and read at most `MAX_KNOWN_HOSTS_BYTES` into a `String`.
fn read_bounded(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut raw = String::new();
    file.take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_string(&mut raw)?;
    if raw.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("known_hosts file exceeds size limit ({MAX_KNOWN_HOSTS_BYTES} bytes)"),
        ));
    }
    Ok(raw)
}

/// Like `read_bounded`, but reads from an already-open file (used while the
/// advisory lock is held).
fn read_bounded_from_handle(file: &mut std::fs::File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut raw = String::new();
    file.take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_string(&mut raw)?;
    if raw.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err(BlinkError::config(format!(
            "known_hosts file exceeds size limit ({MAX_KNOWN_HOSTS_BYTES} bytes)"
        )));
    }
    Ok(raw)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const ED_KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey";
    const ED_KEY_2: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOtherkey";
    const RSA_KEY: &str = "AAAAB3NzaC1yc2EAAA==";

    #[test]
    fn trusted_canonical_form() {
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn trusted_bracketed_non_default_port() {
        let raw = format!("[prod.example.com]:2222 ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 2222, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn trusted_legacy_host_colon_port_form() {
        // Older blink versions wrote `host:port` even for port 22.
        let raw = format!("prod.example.com:22 ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn trusted_case_insensitive_host() {
        let raw = format!("Prod.Example.COM ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn unknown_host() {
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "new.example.com", 22, "ssh-ed25519", "anything");
        assert_eq!(r, KeyStatus::Unknown);
    }

    #[test]
    fn unknown_when_port_differs() {
        // Port 22 entry should not match port 2222.
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 2222, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Unknown);
    }

    #[test]
    fn changed_key_same_algorithm() {
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY_2);
        match r {
            KeyStatus::Changed { stored_key_type, .. } => {
                assert_eq!(stored_key_type, "ssh-ed25519");
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn multi_algorithm_host_matches_presented_algo() {
        // The host has entries for both ed25519 and rsa. Presenting ed25519
        // (which matches its line) must return Trusted, NOT Changed —
        // a different-algorithm line is not a key mismatch.
        let raw = format!(
            "prod.example.com ssh-ed25519 {ED_KEY}\n\
             prod.example.com ssh-rsa {RSA_KEY}\n"
        );
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted, "must match the ed25519 line");
    }

    #[test]
    fn multi_algorithm_host_returns_unknown_for_third_algo() {
        // Host has ed25519 + rsa lines. Presenting ecdsa is Unknown,
        // not Changed (no matching key_type to compare against).
        let raw = format!(
            "prod.example.com ssh-ed25519 {ED_KEY}\n\
             prod.example.com ssh-rsa {RSA_KEY}\n"
        );
        let r = check_in_str(
            &raw,
            "prod.example.com",
            22,
            "ecdsa-sha2-nistp256",
            "anything",
        );
        assert_eq!(r, KeyStatus::Unknown);
    }

    #[test]
    fn trusted_match_after_non_matching_line() {
        // Trusted entry appears AFTER a non-matching line of the same algo.
        // The old code returned on the first host match — this test ensures
        // we now keep scanning.
        let raw = format!(
            "prod.example.com ssh-ed25519 {ED_KEY_2}\n\
             prod.example.com ssh-ed25519 {ED_KEY}\n"
        );
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn skips_comments_and_blanks() {
        let raw = "# blink known hosts\n\nprod.example.com ssh-ed25519 KEY\n";
        let r = check_in_str(raw, "prod.example.com", 22, "ssh-ed25519", "KEY");
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn malformed_lines_ignored() {
        let raw = "not-enough-fields\n\
                   onlytwo fields\n\
                   prod.example.com ssh-ed25519 KEY\n";
        let r = check_in_str(raw, "prod.example.com", 22, "ssh-ed25519", "KEY");
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn canonical_host_strips_port_22() {
        assert_eq!(canonical_host("Host.Example.Com", 22), "host.example.com");
    }

    #[test]
    fn canonical_host_brackets_non_default_port() {
        assert_eq!(
            canonical_host("Host.Example.Com", 2222),
            "[host.example.com]:2222"
        );
    }

    // Note: append() tests touch the real user's known_hosts file and are
    // therefore environment-dependent. The validation paths are covered here
    // without actually appending.

    #[test]
    fn append_rejects_newline_in_host() {
        let r = super::append("evil\nlegit.example.com", 22, "ssh-ed25519", "KEY");
        assert!(r.is_err());
    }

    #[test]
    fn append_rejects_space_in_host() {
        let r = super::append("evil host", 22, "ssh-ed25519", "KEY");
        assert!(r.is_err());
    }

    #[test]
    fn append_rejects_null_byte() {
        let r = super::append("host\x00evil", 22, "ssh-ed25519", "KEY");
        assert!(r.is_err());
    }

    #[test]
    fn append_rejects_carriage_return_in_key() {
        let r = super::append("host", 22, "ssh-ed25519", "KEY\rwith-cr");
        assert!(r.is_err());
    }

    // -- remove_host ------------------------------------------------------
    //
    // `remove_host` itself resolves the real user's known_hosts path, so the
    // filtering logic is tested through `filter_out_host` — the same split
    // `check` / `check_in_str` already uses.

    #[test]
    fn remove_drops_the_canonical_entry() {
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let (out, n) = filter_out_host(&raw, "prod.example.com", 22);
        assert_eq!(n, 1);
        assert_eq!(out, "");
    }

    #[test]
    fn remove_drops_the_bracketed_and_legacy_forms() {
        for stored in ["[prod.example.com]:2222", "prod.example.com:2222"] {
            let raw = format!("{stored} ssh-ed25519 {ED_KEY}\n");
            let (out, n) = filter_out_host(&raw, "prod.example.com", 2222);
            assert_eq!(n, 1, "{stored} should have matched");
            assert_eq!(out, "");
        }
    }

    #[test]
    fn remove_takes_every_algorithm_for_the_host() {
        // A host with both an ed25519 and an rsa entry must be fully
        // forgotten, or the next connect still trips on the leftover.
        let raw = format!(
            "prod.example.com ssh-ed25519 {ED_KEY}\n\
             prod.example.com ssh-rsa {RSA_KEY}\n"
        );
        let (out, n) = filter_out_host(&raw, "prod.example.com", 22);
        assert_eq!(n, 2);
        assert_eq!(out, "");
    }

    #[test]
    fn remove_leaves_other_hosts_untouched() {
        let raw = format!(
            "# blink known hosts\n\
             \n\
             other.example.com ssh-ed25519 {ED_KEY_2}\n\
             prod.example.com ssh-ed25519 {ED_KEY}\n"
        );
        let (out, n) = filter_out_host(&raw, "prod.example.com", 22);
        assert_eq!(n, 1);
        assert!(out.contains("other.example.com"), "neighbour was dropped: {out:?}");
        assert!(out.contains("# blink known hosts"), "comment was dropped: {out:?}");
        assert!(!out.contains("prod.example.com"));
    }

    #[test]
    fn remove_respects_the_port() {
        // A port-22 entry must not be removed by a request for port 2222.
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let (out, n) = filter_out_host(&raw, "prod.example.com", 2222);
        assert_eq!(n, 0, "wrong port must not match");
        assert_eq!(out, raw);
    }

    #[test]
    fn remove_reports_zero_when_nothing_matches() {
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        let (_, n) = filter_out_host(&raw, "absent.example.com", 22);
        assert_eq!(n, 0);
    }

    #[test]
    fn removed_host_is_unknown_again() {
        // The end-to-end property the command exists for: after removal the
        // host reads as Unknown (re-prompt), not Changed (hard reject).
        let raw = format!("prod.example.com ssh-ed25519 {ED_KEY}\n");
        assert!(matches!(
            check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY_2),
            KeyStatus::Changed { .. }
        ));

        let (after, _) = filter_out_host(&raw, "prod.example.com", 22);
        assert_eq!(
            check_in_str(&after, "prod.example.com", 22, "ssh-ed25519", ED_KEY_2),
            KeyStatus::Unknown,
        );
    }
}
