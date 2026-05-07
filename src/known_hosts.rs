//! Known-hosts store: read, check, and append host keys.
//!
//! The file lives at `~/.config/blink/known_hosts` and uses the same
//! line format as OpenSSH's `~/.ssh/known_hosts`:
//!
//! ```text
//! hostname key-type base64-public-key
//! ```
//!
//! Lines beginning with `#` are comments and are preserved on rewrite.
//! Only the exact `hostname` form (no hashing, no patterns) is supported;
//! blink writes entries in this form and only looks them up by exact match.
//!
//! ## Lookup outcomes
//!
//! - [`KeyStatus::Trusted`]   — the host+key pair is in the file.
//! - [`KeyStatus::Unknown`]   — the host has no entry; user should be asked.
//! - [`KeyStatus::Changed`]   — the host has an entry but the key differs;
//!                              this is a hard error (possible MITM).

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{self, BlinkError, Result};
use crate::paths;

/// Maximum size of the known_hosts file accepted on load (1 MiB).
const MAX_KNOWN_HOSTS_BYTES: u64 = 1024 * 1024;

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
    /// Host is in the file but with a different key. Hard reject.
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
// Core operations
// ---------------------------------------------------------------------------

/// Check whether `(host, key_type, key_b64)` is in the known-hosts file.
///
/// - Returns `KeyStatus::Trusted` if an exact line match is found.
/// - Returns `KeyStatus::Changed` if the host is present with a different key.
/// - Returns `KeyStatus::Unknown` if the host is not in the file at all.
///
/// If the file does not exist yet, returns `KeyStatus::Unknown`.
pub fn check(host: &str, key_type: &str, key_b64: &str) -> Result<KeyStatus> {
    let path = known_hosts_path()?;
    let raw = match read_bounded(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(KeyStatus::Unknown),
        Err(e) => return Err(BlinkError::from(e)),
    };
    check_in_str(&raw, host, key_type, key_b64)
}

fn check_in_str(raw: &str, host: &str, key_type: &str, key_b64: &str) -> Result<KeyStatus> {
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let (file_host, file_type, file_b64) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(t), Some(k)) => (h, t, k),
            _ => continue, // malformed line — skip
        };

        if file_host != host {
            continue;
        }
        // Host matched. Check key. Sanitize the stored fields before returning
        // them so callers can display the strings in the TUI without risk of
        // ANSI injection from a tampered file.
        if file_type == key_type && file_b64.trim() == key_b64 {
            return Ok(KeyStatus::Trusted);
        } else {
            return Ok(KeyStatus::Changed {
                stored_key_type: error::sanitize(file_type.to_string()),
                stored_key_b64: error::sanitize(file_b64.trim().to_string()),
            });
        }
    }
    Ok(KeyStatus::Unknown)
}

/// Append a new `host key_type key_b64` line to the known-hosts file.
///
/// Creates the file if it does not exist. If the host+key pair is already
/// present (e.g., from a concurrent connection), the write is skipped so
/// duplicate entries don't accumulate.
pub fn append(host: &str, key_type: &str, key_b64: &str) -> Result<()> {
    // Reject characters that would corrupt the whitespace-delimited format or
    // allow a malicious server to inject trusted entries.
    for (field, value) in [("host", host), ("key_type", key_type), ("key_b64", key_b64)] {
        if value.bytes().any(|b| matches!(b, b'\n' | b'\r' | b'\0')) {
            return Err(BlinkError::config(format!(
                "invalid control character in known_hosts field '{field}'"
            )));
        }
    }
    // Spaces in `host` or `key_type` would silently break the 3-field format
    // when the line is re-parsed, potentially aliasing one entry to another.
    for (field, value) in [("host", host), ("key_type", key_type)] {
        if value.contains(' ') {
            return Err(BlinkError::config(format!(
                "space not allowed in known_hosts field '{field}'"
            )));
        }
    }

    let path = known_hosts_path()?;

    // Skip the write if this exact entry already exists. This prevents
    // duplicates from accumulating when two parallel connections to the same
    // new host both call append() before either reads the (now-updated) file.
    match read_bounded(&path) {
        Ok(raw) => {
            if matches!(check_in_str(&raw, host, key_type, key_b64), Ok(KeyStatus::Trusted)) {
                return Ok(());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(BlinkError::from(e)),
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{host} {key_type} {key_b64}")?;
    Ok(())
}

/// Remove all lines for `host` from the known-hosts file.
///
/// Used when the user explicitly replaces a key (not currently exposed in UI,
/// but useful for programmatic cleanup and future "update key" flows).
#[allow(dead_code)]
pub fn remove_host(host: &str) -> Result<()> {
    let path = known_hosts_path()?;

    // Read the file, treating NotFound as "nothing to do". This avoids the
    // TOCTOU race of exists()-then-read().
    let raw = match read_bounded(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(BlinkError::from(e)),
    };

    let filtered: String = raw
        .lines()
        .filter(|line| {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                return true; // keep comments and blanks
            }
            let host_field = t.splitn(2, ' ').next().unwrap_or("");
            host_field != host
        })
        .map(|l| format!("{l}\n"))
        .collect();

    // Write atomically: temp file + rename so a crash mid-write never
    // corrupts the file (which would force the user to re-verify all hosts).
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &filtered)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Format a key for display: truncate to 24 *characters* with an ellipsis
/// so it fits on one line in the modal.
pub fn display_key(key_b64: &str) -> String {
    // Count chars, not bytes, to avoid a panic if the known_hosts file has
    // been tampered with to contain multi-byte UTF-8 in the key column.
    let mut chars = key_b64.chars().peekable();
    let prefix: String = chars.by_ref().take(24).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Open `path` and read at most `MAX_KNOWN_HOSTS_BYTES` into a `String`.
///
/// Returns `io::Error` (including `NotFound`) so callers can handle the
/// not-present case without an extra existence check (which would be a TOCTOU
/// race).
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# blink known hosts
prod.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey
dev.example.com ssh-rsa AAAAB3NzaC1yc2EAAA==
";

    #[test]
    fn trusted() {
        let r = check_in_str(SAMPLE, "prod.example.com", "ssh-ed25519", "AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey");
        assert_eq!(r.unwrap(), KeyStatus::Trusted);
    }

    #[test]
    fn unknown_host() {
        let r = check_in_str(SAMPLE, "new.example.com", "ssh-ed25519", "anything");
        assert_eq!(r.unwrap(), KeyStatus::Unknown);
    }

    #[test]
    fn changed_key() {
        let r = check_in_str(SAMPLE, "prod.example.com", "ssh-ed25519", "DIFFERENT");
        assert!(matches!(r.unwrap(), KeyStatus::Changed { .. }));
    }

    #[test]
    fn changed_key_type() {
        let r = check_in_str(SAMPLE, "prod.example.com", "ssh-rsa", "AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey");
        assert!(matches!(r.unwrap(), KeyStatus::Changed { .. }));
    }

    #[test]
    fn skips_comments_and_blanks() {
        let r = check_in_str(SAMPLE, "#", "any", "any");
        assert_eq!(r.unwrap(), KeyStatus::Unknown);
    }

    #[test]
    fn display_key_short() {
        assert_eq!(display_key("ABC"), "ABC");
    }

    #[test]
    fn display_key_exact_limit() {
        let k = "A".repeat(24);
        assert_eq!(display_key(&k), k);
    }

    #[test]
    fn display_key_truncated() {
        let k = "A".repeat(30);
        let d = display_key(&k);
        assert!(d.ends_with('…'));
        assert_eq!(d.chars().count(), 25); // 24 chars + ellipsis
    }

    #[test]
    fn display_key_multibyte_no_panic() {
        // Simulate a tampered file with multi-byte chars in the key column.
        let k = "日".repeat(30);
        let d = display_key(&k);
        assert!(d.ends_with('…'));
    }

    #[test]
    fn append_rejects_newline_in_host() {
        let r = super::append("evil\nlegit.example.com", "ssh-ed25519", "KEY");
        assert!(r.is_err());
    }

    #[test]
    fn append_rejects_space_in_host() {
        let r = super::append("evil host", "ssh-ed25519", "KEY");
        assert!(r.is_err());
    }

    #[test]
    fn append_rejects_null_byte() {
        let r = super::append("host\x00evil", "ssh-ed25519", "KEY");
        assert!(r.is_err());
    }
}
