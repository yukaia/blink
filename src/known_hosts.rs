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
//! - [`KeyStatus::Changed`] too when the host has lines, but none for the
//!   presented keytype. blink asks for the stored keytypes first (see
//!   [`stored_key_types`]), so a server that still holds one of those keys
//!   presents it; one that presents another type instead is what a man in
//!   the middle without the real keys looks like. Prompting as if this were
//!   a first connection would let one habitual "accept" hand it over.
//! - [`KeyStatus::Unknown`] — no line for the host at all. Ask the user.
//!
//! A host with both `ssh-ed25519` and `ssh-rsa` entries does not flag
//! `Changed` when only one of them is presented — that's normal
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
// Per-session trust ("accept once")
// ---------------------------------------------------------------------------

/// Host keys the user accepted for the lifetime of one connected session,
/// without persisting them to the known-hosts file.
///
/// "Trust once" has to mean once per *session*, not once per TCP connection.
/// A connected session opens many: the interactive one, plus one per
/// parallel-transfer worker in the dispatcher's pool. Each runs its own
/// [`check`] against the known-hosts file, finds nothing (that is what
/// accept-once means), and would ask again — mid-transfer, once per worker.
///
/// Cloning shares the underlying set, so a decision made on any connection is
/// immediately visible to the rest. Dropping the last clone — which happens on
/// disconnect — forgets the decision, which is the scope the prompt promised.
#[derive(Clone, Default)]
pub struct SessionTrust {
    accepted: std::sync::Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
}

impl SessionTrust {
    pub fn new() -> Self {
        Self::default()
    }

    /// Host is case-folded to match [`check`]'s lookup semantics; the key
    /// blob and algorithm are compared verbatim.
    fn entry(host: &str, port: u16, key_type: &str, key_b64: &str) -> String {
        format!(
            "{}\u{0}{port}\u{0}{key_type}\u{0}{key_b64}",
            host.to_ascii_lowercase()
        )
    }

    /// Record that the user accepted this key for the current session.
    pub fn trust(&self, host: &str, port: u16, key_type: &str, key_b64: &str) {
        self.accepted
            .lock()
            .insert(Self::entry(host, port, key_type, key_b64));
    }

    /// Whether this exact key was already accepted for the current session.
    pub fn is_trusted(&self, host: &str, port: u16, key_type: &str, key_b64: &str) -> bool {
        self.accepted
            .lock()
            .contains(&Self::entry(host, port, key_type, key_b64))
    }
}

impl std::fmt::Debug for SessionTrust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the accepted keys themselves.
        f.debug_struct("SessionTrust")
            .field("accepted", &self.accepted.lock().len())
            .finish()
    }
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
    // Keep scanning all matching lines.
    // - Any line whose (host, key_type, key_b64) all match → Trusted.
    // - Else if any line with this (host, key_type) has a different blob →
    //   Changed, naming that line.
    // - Else if the host has a line of another type → Changed, naming the
    //   first such line. See the module docs.
    // - Else → Unknown.
    let mut changed: Option<KeyStatus> = None;
    let mut other_type: Option<KeyStatus> = None;

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
            // Normal for a host with several keys, as long as one of the
            // presented type matches; remembered in case none does.
            if other_type.is_none() {
                other_type = Some(KeyStatus::Changed {
                    stored_key_type: error::sanitize(file_type.to_string()),
                    stored_key_b64: error::sanitize(file_b64.to_string()),
                });
            }
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

    changed.or(other_type).unwrap_or(KeyStatus::Unknown)
}

/// The keytypes stored for `host:port`, in file order, each once.
///
/// The SSH client puts these first in its host-key preference, as OpenSSH
/// does, so a server holding several keys presents one blink can check —
/// which is what lets [`check`] treat any other type as `Changed`. A read
/// error yields an empty list: the preference is only an ordering, and
/// [`check`] fails closed on the same error.
pub fn stored_key_types(host: &str, port: u16) -> Vec<String> {
    match known_hosts_path().and_then(|p| read_bounded(&p).map_err(BlinkError::from)) {
        Ok(raw) => stored_key_types_in_str(&raw, host, port),
        Err(_) => Vec::new(),
    }
}

fn stored_key_types_in_str(raw: &str, host: &str, port: u16) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let (Some(file_host), Some(file_type), Some(_)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if host_matches(file_host, host, port) && !types.iter().any(|t| t == file_type) {
            types.push(file_type.to_string());
        }
    }
    types
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
    // A file whose last line has no newline — hand-edited, say — would have
    // this entry glued onto that line, changing its key blob: that host is
    // then rejected as "key changed" and this one is never found. End the
    // line first. An empty file needs nothing.
    if !raw.is_empty() && !raw.ends_with('\n') {
        writeln!(file)?;
    }
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
mod trust_tests {
    use super::SessionTrust;

    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey";

    #[test]
    fn an_untrusted_key_is_not_trusted() {
        let t = SessionTrust::new();
        assert!(!t.is_trusted("h", 22, "ssh-ed25519", KEY));
    }

    #[test]
    fn a_trusted_key_is_recognised_again() {
        // The whole point: "trust once" has to mean once per session, not
        // once per TCP connection. Each parallel-transfer worker opens its
        // own connection, and without this every one of them re-prompted.
        let t = SessionTrust::new();
        t.trust("h", 22, "ssh-ed25519", KEY);
        assert!(t.is_trusted("h", 22, "ssh-ed25519", KEY));
    }

    #[test]
    fn trust_is_scoped_to_the_exact_key() {
        let t = SessionTrust::new();
        t.trust("h", 22, "ssh-ed25519", KEY);
        assert!(
            !t.is_trusted("h", 22, "ssh-ed25519", "OTHERKEY"),
            "different key"
        );
        assert!(
            !t.is_trusted("h", 22, "ssh-rsa", KEY),
            "different algorithm"
        );
        assert!(
            !t.is_trusted("h", 2222, "ssh-ed25519", KEY),
            "different port"
        );
        assert!(
            !t.is_trusted("other", 22, "ssh-ed25519", KEY),
            "different host"
        );
    }

    #[test]
    fn clones_share_one_set() {
        // Workers get clones; a decision made on one must be visible to all.
        let t = SessionTrust::new();
        let worker = t.clone();
        t.trust("h", 22, "ssh-ed25519", KEY);
        assert!(worker.is_trusted("h", 22, "ssh-ed25519", KEY));
    }

    #[test]
    fn separate_stores_do_not_share_trust() {
        // A new connected session starts from scratch — an accept-once from
        // a previous session must not carry over.
        let a = SessionTrust::new();
        let b = SessionTrust::new();
        a.trust("h", 22, "ssh-ed25519", KEY);
        assert!(!b.is_trusted("h", 22, "ssh-ed25519", KEY));
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        let t = SessionTrust::new();
        t.trust("Host.Example.COM", 22, "ssh-ed25519", KEY);
        assert!(t.is_trusted("host.example.com", 22, "ssh-ed25519", KEY));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey";
    const ED_KEY_2: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOtherkey";
    const RSA_KEY: &str = "AAAAB3NzaC1yc2EAAA==";

    // -- appending after a line with no newline ------------------------------
    //
    // `append` used to write its entry straight after whatever the file ended
    // with. A hand-edited file missing its final newline got the new entry
    // glued onto its last line, which changed that host's key blob: the host
    // was then hard-rejected as "key changed", and the new one not found.

    fn write_known_hosts(contents: &str) -> std::path::PathBuf {
        let path = known_hosts_path().unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn appending_after_a_last_line_without_a_newline_keeps_both_entries() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts(&format!("a.example ssh-ed25519 {ED_KEY}"));

        append("b.example", 22, "ssh-ed25519", ED_KEY_2).unwrap();

        assert_eq!(
            check("a.example", 22, "ssh-ed25519", ED_KEY).unwrap(),
            KeyStatus::Trusted,
            "the existing entry must survive intact",
        );
        assert_eq!(
            check("b.example", 22, "ssh-ed25519", ED_KEY_2).unwrap(),
            KeyStatus::Trusted,
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("a.example ssh-ed25519 {ED_KEY}\nb.example ssh-ed25519 {ED_KEY_2}\n"),
            "one newline between them, and no blank line",
        );
    }

    #[test]
    fn appending_to_an_empty_file_starts_on_its_first_line() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts("");

        append("b.example", 22, "ssh-ed25519", ED_KEY_2).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("b.example ssh-ed25519 {ED_KEY_2}\n"),
        );
    }

    #[test]
    fn appending_after_a_newline_adds_no_blank_line() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts(&format!("a.example ssh-ed25519 {ED_KEY}\n"));

        append("b.example", 22, "ssh-ed25519", ED_KEY_2).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("a.example ssh-ed25519 {ED_KEY}\nb.example ssh-ed25519 {ED_KEY_2}\n"),
        );
    }

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
            KeyStatus::Changed {
                stored_key_type, ..
            } => {
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
    fn a_known_host_presenting_only_a_new_algorithm_is_changed() {
        // Host has ed25519 + rsa lines. Presenting ecdsa means the server
        // offered none of the stored types, even though blink asks for those
        // first — the downgrade a man in the middle without the real keys
        // would attempt. It must read as Changed, not as a first connection.
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
        match r {
            KeyStatus::Changed {
                stored_key_type, ..
            } => assert_eq!(stored_key_type, "ssh-ed25519", "the first stored line"),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn a_same_algorithm_mismatch_is_reported_over_an_other_algorithm_one() {
        let raw = format!(
            "prod.example.com ssh-rsa {RSA_KEY}\n\
             prod.example.com ssh-ed25519 {ED_KEY}\n"
        );
        match check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY_2) {
            KeyStatus::Changed {
                stored_key_type, ..
            } => assert_eq!(stored_key_type, "ssh-ed25519"),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn another_hosts_algorithms_do_not_make_a_key_changed() {
        let raw = format!("other.example.com ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-rsa", RSA_KEY);
        assert_eq!(r, KeyStatus::Unknown);
    }

    #[test]
    fn stored_key_types_lists_each_type_once_for_that_host_only() {
        let raw = format!(
            "prod.example.com ssh-rsa {RSA_KEY}\n\
             other.example.com ecdsa-sha2-nistp256 AAAA\n\
             [prod.example.com]:2222 ssh-ed25519 {ED_KEY}\n\
             prod.example.com ssh-ed25519 {ED_KEY}\n\
             prod.example.com ssh-ed25519 {ED_KEY_2}\n"
        );
        assert_eq!(
            stored_key_types_in_str(&raw, "prod.example.com", 22),
            vec!["ssh-rsa".to_string(), "ssh-ed25519".to_string()],
        );
        assert!(stored_key_types_in_str(&raw, "absent.example.com", 22).is_empty());
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

    // Note: append() resolves paths::root_dir(), which under test redirects
    // to the test home rather than the real user's known_hosts file — so
    // end-to-end coverage of append() (writing an entry and reading it back)
    // is possible now and simply hasn't been written yet. Only the
    // validation paths are covered here for the moment.

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
    // `remove_host` resolves paths::root_dir(), which under test redirects
    // to the test home, not the real user's known_hosts path — so
    // end-to-end coverage of remove_host() is possible now and simply
    // hasn't been written yet. For the moment the filtering logic is tested
    // through `filter_out_host` — the same split `check` / `check_in_str`
    // already uses.

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
        assert!(
            out.contains("other.example.com"),
            "neighbour was dropped: {out:?}"
        );
        assert!(
            out.contains("# blink known hosts"),
            "comment was dropped: {out:?}"
        );
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
