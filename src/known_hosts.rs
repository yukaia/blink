//! Known-hosts store: read, check, and append host keys.
//!
//! The file is `known_hosts` in blink's config directory (see
//! [`paths::root_dir`]) and uses the same line format as OpenSSH's
//! `~/.ssh/known_hosts`, so an OpenSSH file can be copied in:
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
//! ## Other forms read
//!
//! - Hashed names (`|1|salt|hash`), as OpenSSH writes with
//!   `HashKnownHosts=yes`: HMAC-SHA1 of the host's canonical form, keyed
//!   by the salt.
//! - Wildcard patterns: `*` for any run of characters, `?` for one, matched
//!   against the canonical form (so `[*.example.com]:2222` for a port).
//! - Comma-separated host lists, and `!`-negated names and patterns.
//!
//! `@cert-authority` lines are ignored, since blink refuses host
//! certificates outright. blink writes plain names only.
//!
//! Removing a host ([`remove_host`]) takes out its plain and hashed names,
//! never a pattern: forgetting `a.example.com` must not forget every host
//! `*.example.com` vouches for.
//!
//! ## Concurrency
//!
//! Every change, [`append`] and [`remove_host`] alike, holds an exclusive
//! lock on `known_hosts.lock` beside the file. The lock can't live on
//! `known_hosts` itself: removing rewrites the file and renames the new one
//! into place, and a lock on the old file then guards nothing.
//!
//! ## Revocation
//!
//! A `@revoked` line refuses its key for **every** host, not only the ones
//! it lists: a revoked key is a compromised one, and no other line can
//! vouch for it.
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

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
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
    /// The presented key is on a `@revoked` line. Hard reject, whatever
    /// other lines say about the host.
    Revoked,
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
///
/// Accepting a key vouches for the host for the session, as a known_hosts
/// entry does: [`SessionTrust::check`] answers the way [`check`] does, so any
/// other key from that host is a changed key, not a new host to ask about.
#[derive(Clone, Default)]
pub struct SessionTrust {
    accepted: std::sync::Arc<parking_lot::Mutex<HashMap<TrustedHost, TrustedKey>>>,
}

/// (case-folded host, port).
type TrustedHost = (String, u16);
/// (key type, base64 key) accepted for a [`TrustedHost`].
type TrustedKey = (String, String);

impl SessionTrust {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the user accepted this key for the current session.
    pub fn trust(&self, host: &str, port: u16, key_type: &str, key_b64: &str) {
        self.accepted.lock().insert(
            (host.to_ascii_lowercase(), port),
            (key_type.to_string(), key_b64.to_string()),
        );
    }

    /// Check a presented key against what this session accepted. Host is
    /// case-folded to match [`check`]'s lookup semantics; the key blob and
    /// algorithm are compared verbatim.
    pub fn check(&self, host: &str, port: u16, key_type: &str, key_b64: &str) -> KeyStatus {
        match self.accepted.lock().get(&(host.to_ascii_lowercase(), port)) {
            None => KeyStatus::Unknown,
            Some((t, k)) if t == key_type && k == key_b64 => KeyStatus::Trusted,
            // Sanitized as [`check`] sanitizes what it read from the file:
            // whoever shows a `Changed` can count on that either way.
            Some((t, k)) => KeyStatus::Changed {
                stored_key_type: error::sanitize(t.clone()),
                stored_key_b64: error::sanitize(k.clone()),
            },
        }
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

/// Take the lock every change to the file holds; see the module docs.
/// Released when the returned file is dropped.
fn lock_store() -> Result<fs::File> {
    let path = paths::root_dir()?.join("known_hosts.lock");
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&path)
        .map_err(|e| BlinkError::config(format!("{}: {e}", path.display())))?;
    // Blocks until acquired: the held regions are a read and a small write.
    // `File::lock` is std's (stable since Rust 1.89), released on drop.
    file.lock()
        .map_err(|e| BlinkError::config(format!("{}: lock: {e}", path.display())))?;
    Ok(file)
}

/// A read of the file that failed for a reason other than its absence,
/// naming the file: "stream did not contain valid UTF-8" alone said
/// nothing about where to look.
fn read_error(path: &Path, e: std::io::Error) -> BlinkError {
    BlinkError::config(format!("could not read {}: {e}", path.display()))
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

/// Whether wildcard patterns in the file may match. Looking a host up
/// takes them; removing one doesn't, so that forgetting `a.example.com`
/// leaves `*.example.com` alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Patterns {
    Match,
    Literal,
}

/// Whether `file_host` (one name from a known_hosts host list) refers to
/// `(host, port)`. Accepts the canonical form, a hashed name, a wildcard
/// pattern (with [`Patterns::Match`]), and the legacy `host:port` form —
/// all ignoring ASCII case.
fn host_matches(file_host: &str, host: &str, port: u16, patterns: Patterns) -> bool {
    let canonical = canonical_host(host, port);
    if let Some(hashed) = file_host.strip_prefix("|1|") {
        return hashed_matches(hashed, &canonical);
    }
    if file_host.eq_ignore_ascii_case(&canonical) {
        return true;
    }
    if file_host.contains(['*', '?']) {
        return patterns == Patterns::Match && glob_matches(file_host, &canonical);
    }
    // Not for an IPv6 host: `::1:22` is legacy `::1` on port 22 and
    // canonical `::1:22` on port 22 alike, so reading it both ways would
    // let a key stored for one address vouch for the other. blink never
    // wrote the legacy form for a host it could not connect to without
    // brackets, so nothing that was trusted correctly stops matching.
    if host.contains(':') {
        return false;
    }
    let legacy = legacy_host(host, port);
    file_host.eq_ignore_ascii_case(&legacy)
}

/// Whether a hashed name's `salt|hash` (both base64) is `canonical`:
/// OpenSSH's `HashKnownHosts` stores HMAC-SHA1(key = salt, the canonical
/// lowercased name). Anything that doesn't decode matches nothing.
fn hashed_matches(salt_and_hash: &str, canonical: &str) -> bool {
    use base64::Engine as _;
    use hmac::{Hmac, KeyInit, Mac};

    let Some((salt, hash)) = salt_and_hash.split_once('|') else {
        return false;
    };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(salt), Ok(hash)) = (b64.decode(salt), b64.decode(hash)) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<sha1::Sha1>::new_from_slice(&salt) else {
        return false;
    };
    // Already lowercased: `canonical_host` folds case, as OpenSSH does
    // before hashing.
    mac.update(canonical.as_bytes());
    // Constant-time, though nothing secret is compared: it costs nothing.
    mac.verify_slice(&hash).is_ok()
}

/// Whether `text` fits `pattern`, where `*` stands for any run of
/// characters (none included) and `?` for exactly one, ignoring ASCII case.
/// OpenSSH's `match_pattern`.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0, 0);
    // Where the last `*` was, and how much of the text it had taken.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // Let the last `*` take one more character and try again.
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// One non-comment line of a known_hosts file.
struct Entry<'a> {
    /// `@revoked`, `@cert-authority`, or another `@` marker.
    marker: Option<&'a str>,
    /// The comma-separated host list.
    hosts: &'a str,
    key_type: &'a str,
    key_b64: &'a str,
}

/// Split a line into its fields, OpenSSH-style: any run of spaces or tabs
/// separates them, and anything after the key is a comment. `None` for
/// blanks, comments, and lines too short to be an entry.
fn parse_line(line: &str) -> Option<Entry<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields = line.split_ascii_whitespace();
    let first = fields.next()?;
    let (marker, hosts) = if first.starts_with('@') {
        (Some(first), fields.next()?)
    } else {
        (None, first)
    };
    Some(Entry {
        marker,
        hosts,
        key_type: fields.next()?,
        key_b64: fields.next()?,
    })
}

/// Whether a comma-separated host list names `(host, port)`: some name
/// matches it and no `!`-negated name does. See the module docs for the
/// forms a name can take.
fn hosts_match(hosts: &str, host: &str, port: u16, patterns: Patterns) -> bool {
    let mut matched = false;
    for name in hosts.split(',') {
        if let Some(negated) = name.strip_prefix('!') {
            if host_matches(negated, host, port, patterns) {
                return false;
            }
        } else if host_matches(name, host, port, patterns) {
            matched = true;
        }
    }
    matched
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
        Err(e) => return Err(read_error(&path, e)),
    };
    Ok(check_in_str(&raw, host, port, key_type, key_b64))
}

fn check_in_str(raw: &str, host: &str, port: u16, key_type: &str, key_b64: &str) -> KeyStatus {
    // - A `@revoked` line carrying the presented key → Revoked, for any
    //   host: a revoked key is compromised and vouches for nothing.
    // - Else any line whose (host, key_type, key_b64) all match → Trusted.
    // - Else if any line with this (host, key_type) has a different blob →
    //   Changed, naming that line.
    // - Else if the host has a line of another type → Changed, naming the
    //   first such line. See the module docs.
    // - Else → Unknown.
    let entries: Vec<Entry<'_>> = raw.lines().filter_map(parse_line).collect();

    if entries
        .iter()
        .any(|e| e.marker == Some("@revoked") && e.key_type == key_type && e.key_b64 == key_b64)
    {
        return KeyStatus::Revoked;
    }

    let mut changed: Option<KeyStatus> = None;
    let mut other_type: Option<KeyStatus> = None;

    // Other markers (`@cert-authority`) are not host keys: blink refuses
    // host certificates before it gets here.
    for e in entries.iter().filter(|e| e.marker.is_none()) {
        if !hosts_match(e.hosts, host, port, Patterns::Match) {
            continue;
        }
        let stored = || KeyStatus::Changed {
            stored_key_type: error::sanitize(e.key_type.to_string()),
            stored_key_b64: error::sanitize(e.key_b64.to_string()),
        };
        if e.key_type != key_type {
            // Normal for a host with several keys, as long as one of the
            // presented type matches; remembered in case none does.
            other_type.get_or_insert_with(stored);
            continue;
        }
        if e.key_b64 == key_b64 {
            return KeyStatus::Trusted;
        }
        // Same host, same algorithm, different blob — remember as Changed
        // (but keep scanning in case a later line is a Trusted match).
        changed.get_or_insert_with(stored);
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
    for e in raw.lines().filter_map(parse_line) {
        if e.marker.is_none()
            && hosts_match(e.hosts, host, port, Patterns::Match)
            && !types.iter().any(|t| t == e.key_type)
        {
            types.push(e.key_type.to_string());
        }
    }
    types
}

/// What [`append`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// The entry was written.
    Added,
    /// The file already trusts this key; nothing was written.
    AlreadyTrusted,
    /// The file now holds something else for this host — a different key,
    /// saved by another blink while the user was deciding, or a revocation
    /// of this one. Nothing was written: two keys for one host would both
    /// be trusted from then on, and one of them is suspect.
    Conflict(KeyStatus),
}

/// Append a new entry to the known-hosts file, in the canonical form
/// (bare host for port 22, `[host]:port` otherwise).
///
/// Creates the file if it does not exist. Holds the store's lock (see the
/// module docs) across the check and the write, so two blinks accepting
/// the same host can't both write, and the decision is made against the
/// file as it is now rather than as it was when the prompt opened. The
/// line goes in one write and is synced: written piecemeal, a crash could
/// leave a key cut short, and that host would read as a changed key.
pub fn append(host: &str, port: u16, key_type: &str, key_b64: &str) -> Result<Saved> {
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
    // `parse_line` splits fields on any whitespace and drops a line with
    // fewer than three, so whitespace in a field, or an empty one, writes a
    // line that reads back as another entry or as none.
    for (field, value) in [("host", host), ("key_type", key_type), ("key_b64", key_b64)] {
        if value.is_empty() || value.contains(char::is_whitespace) {
            return Err(BlinkError::config(format!(
                "known_hosts field '{field}' must not be empty or contain whitespace"
            )));
        }
    }
    // A comma separates the names of a host list, so a host holding one
    // would be stored as two hosts, neither of them this one.
    if host.contains(',') {
        return Err(BlinkError::config(
            "comma not allowed in known_hosts field 'host'",
        ));
    }
    // Written bare, a host starting with one of these reads back as a
    // comment, a negation, a marker or a hashed name, and one holding a
    // wildcard as a pattern for other hosts: never this host, so it would
    // be asked about, and a dead line added, on every connect.
    if host.starts_with(['#', '!', '@', '|']) || host.contains(['*', '?']) {
        return Err(BlinkError::config(format!(
            "host `{}` can't be stored in known_hosts",
            error::sanitize(host.to_string())
        )));
    }

    let _lock = lock_store()?;
    let path = known_hosts_path()?;
    let raw = match read_bounded(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(read_error(&path, e)),
    };
    match check_in_str(&raw, host, port, key_type, key_b64) {
        KeyStatus::Unknown => {}
        KeyStatus::Trusted => return Ok(Saved::AlreadyTrusted),
        other => return Ok(Saved::Conflict(other)),
    }

    // A file whose last line has no newline — hand-edited, say — would have
    // this entry glued onto that line, changing its key blob: that host is
    // then rejected as "key changed" and this one is never found. End the
    // line first. An empty file needs nothing.
    let mut line = String::new();
    if !raw.is_empty() && !raw.ends_with('\n') {
        line.push('\n');
    }
    line.push_str(&format!("{stored_host} {key_type} {key_b64}\n"));

    // Created 0600 on Unix, as `paths::atomic_write` creates every other
    // file.
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&path)?;
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    if raw.is_empty() {
        // Possibly a new file: make its name durable too.
        paths::sync_parent_dir(&path)?;
    }
    Ok(Saved::Added)
}

/// Remove every entry for `(host, port)` from the known-hosts file.
///
/// Returns how many lines were removed, so the caller can tell "forgot the
/// key" apart from "nothing matched" — the latter usually means the user
/// named a different host form than the one that was stored (a bare host
/// when the entry is `[host]:port`, or vice versa).
///
/// Matching accepts the same forms as [`check`] — the bare host for port
/// 22, `[host]:port`, the legacy `host:port`, and hashed names — except
/// wildcard patterns, which are left alone; see the module docs.
pub fn remove_host(host: &str, port: u16) -> Result<usize> {
    // Held across the read and the rewrite, so an `append` can't land in
    // between and be lost when the rewritten file replaces this one.
    let _lock = lock_store()?;
    let path = known_hosts_path()?;

    let raw = match read_bounded(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(read_error(&path, e)),
    };

    let (filtered, removed) = filter_out_host(&raw, host, port);

    // Nothing to do — don't rewrite the file just to produce identical
    // content.
    if removed == 0 {
        return Ok(0);
    }

    // Atomic + durable write, as for every other file blink owns: a power
    // loss can't leave a zero-byte known_hosts, which would silently
    // downgrade every stored host to "unknown". See `paths::atomic_write`.
    paths::atomic_write(&path, |f| f.write_all(filtered.as_bytes()))?;
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Drop every entry matching `(host, port)` from `raw`, returning the
/// rewritten contents and how many lines it changed.
///
/// Comments and blank lines are preserved, and lines whose host field names a
/// different host or a different port are left alone — removing a key must
/// not disturb neighbouring entries. A line listing other names as well
/// keeps them and loses only the ones naming this host. Marker lines are
/// never touched: forgetting a host must not un-revoke a key. Nor are
/// wildcard patterns ([`Patterns::Literal`]). Each line keeps its own
/// ending, so a CRLF file stays CRLF.
fn filter_out_host(raw: &str, host: &str, port: u16) -> (String, usize) {
    let mut changed = 0usize;
    let mut out = String::with_capacity(raw.len());
    for segment in raw.split_inclusive('\n') {
        let (line, ending) = if let Some(l) = segment.strip_suffix("\r\n") {
            (l, "\r\n")
        } else if let Some(l) = segment.strip_suffix('\n') {
            (l, "\n")
        } else {
            (segment, "")
        };
        let Some(e) = parse_line(line).filter(|e| e.marker.is_none()) else {
            out.push_str(segment);
            continue;
        };
        if !hosts_match(e.hosts, host, port, Patterns::Literal) {
            out.push_str(segment);
            continue;
        }
        changed += 1;
        let kept: Vec<&str> = e
            .hosts
            .split(',')
            .filter(|n| !host_matches(n, host, port, Patterns::Literal))
            .collect();
        // Nothing left but negations (or nothing at all) names no host.
        if kept.iter().all(|n| n.starts_with('!')) {
            continue;
        }
        let rest = line.trim_start()[e.hosts.len()..].trim_start();
        out.push_str(&kept.join(","));
        out.push(' ');
        out.push_str(rest);
        out.push_str(ending);
    }
    (out, changed)
}

/// Open `path` and read at most `MAX_KNOWN_HOSTS_BYTES` into a `String`.
///
/// Bytes that aren't UTF-8 become U+FFFD rather than failing the read: one
/// Latin-1 byte in a comment refused every SSH connection. That's safe,
/// because host names and keys are ASCII, so a line with a bad byte in
/// them simply never matches.
fn read_bounded(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut raw = Vec::new();
    file.take(MAX_KNOWN_HOSTS_BYTES + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > MAX_KNOWN_HOSTS_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("known_hosts file exceeds size limit ({MAX_KNOWN_HOSTS_BYTES} bytes)"),
        ));
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod trust_tests {
    use super::{KeyStatus, SessionTrust};

    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIGoodkey";

    #[test]
    fn an_untrusted_key_is_unknown() {
        let t = SessionTrust::new();
        assert_eq!(t.check("h", 22, "ssh-ed25519", KEY), KeyStatus::Unknown);
    }

    #[test]
    fn a_trusted_key_is_recognised_again() {
        // The whole point: "trust once" has to mean once per session, not
        // once per TCP connection. Each parallel-transfer worker opens its
        // own connection, and without this every one of them re-prompted.
        let t = SessionTrust::new();
        t.trust("h", 22, "ssh-ed25519", KEY);
        assert_eq!(t.check("h", 22, "ssh-ed25519", KEY), KeyStatus::Trusted);
    }

    /// Accepting a key once vouches for that host, as a known_hosts entry
    /// does: another key from it later in the session is the man in the
    /// middle the first prompt was guarding against, and must be refused,
    /// not offered as a host seen for the first time.
    #[test]
    fn another_key_from_a_host_trusted_once_is_a_changed_key() {
        let t = SessionTrust::new();
        t.trust("h", 22, "ssh-ed25519", KEY);
        let changed = KeyStatus::Changed {
            stored_key_type: "ssh-ed25519".into(),
            stored_key_b64: KEY.into(),
        };
        assert_eq!(
            t.check("h", 22, "ssh-ed25519", "OTHERKEY"),
            changed,
            "different key"
        );
        assert_eq!(
            t.check("H", 22, "ssh-rsa", KEY),
            changed,
            "different algorithm"
        );
    }

    #[test]
    fn trust_is_scoped_to_the_host_and_port() {
        let t = SessionTrust::new();
        t.trust("h", 22, "ssh-ed25519", KEY);
        assert_eq!(
            t.check("h", 2222, "ssh-ed25519", KEY),
            KeyStatus::Unknown,
            "different port"
        );
        assert_eq!(
            t.check("other", 22, "ssh-ed25519", KEY),
            KeyStatus::Unknown,
            "different host"
        );
    }

    #[test]
    fn clones_share_one_set() {
        // Workers get clones; a decision made on one must be visible to all.
        let t = SessionTrust::new();
        let worker = t.clone();
        t.trust("h", 22, "ssh-ed25519", KEY);
        assert_eq!(
            worker.check("h", 22, "ssh-ed25519", KEY),
            KeyStatus::Trusted
        );
    }

    #[test]
    fn separate_stores_do_not_share_trust() {
        // A new connected session starts from scratch — an accept-once from
        // a previous session must not carry over.
        let a = SessionTrust::new();
        let b = SessionTrust::new();
        a.trust("h", 22, "ssh-ed25519", KEY);
        assert_eq!(b.check("h", 22, "ssh-ed25519", KEY), KeyStatus::Unknown);
    }

    #[test]
    fn host_matching_is_case_insensitive() {
        let t = SessionTrust::new();
        t.trust("Host.Example.COM", 22, "ssh-ed25519", KEY);
        assert_eq!(
            t.check("host.example.com", 22, "ssh-ed25519", KEY),
            KeyStatus::Trusted
        );
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

    // --- remove_host: the file itself, not only the text it is given ---

    #[test]
    fn removing_a_host_takes_every_key_it_has_and_leaves_the_rest() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts(&format!(
            "# mine\n\
             a.example ssh-ed25519 {ED_KEY}\n\
             a.example ssh-rsa RSAKEY\n\
             b.example ssh-ed25519 {ED_KEY_2}\n"
        ));

        assert_eq!(remove_host("a.example", 22).unwrap(), 2);

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("# mine\nb.example ssh-ed25519 {ED_KEY_2}\n"),
        );
        assert_eq!(
            check("a.example", 22, "ssh-ed25519", ED_KEY).unwrap(),
            KeyStatus::Unknown,
            "the next connect asks again",
        );
    }

    #[test]
    fn removing_a_host_is_per_port() {
        let _home = crate::paths::test_home();
        let line = format!("[a.example]:2222 ssh-ed25519 {ED_KEY}\n");
        let path = write_known_hosts(&line);

        assert_eq!(remove_host("a.example", 22).unwrap(), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), line);

        assert_eq!(remove_host("a.example", 2222).unwrap(), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    /// Nothing to remove means nothing is written: the file keeps its exact
    /// bytes, here a last line with no newline, which a rewrite would add.
    #[test]
    fn removing_a_host_that_isnt_there_leaves_the_file_untouched() {
        let _home = crate::paths::test_home();
        let contents = format!("b.example ssh-ed25519 {ED_KEY_2}");
        let path = write_known_hosts(&contents);

        assert_eq!(remove_host("a.example", 22).unwrap(), 0);

        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn removing_a_host_with_no_known_hosts_file_removes_nothing() {
        let _home = crate::paths::test_home();
        let path = known_hosts_path().unwrap();

        assert_eq!(remove_host("a.example", 22).unwrap(), 0);

        assert!(!path.exists(), "and doesn't create one");
    }

    /// Forgetting a host must not un-revoke a key.
    #[test]
    fn removing_a_host_keeps_its_revoked_keys_revoked() {
        let _home = crate::paths::test_home();
        write_known_hosts(&format!(
            "@revoked a.example ssh-ed25519 {ED_KEY}\n\
             a.example ssh-ed25519 {ED_KEY_2}\n"
        ));

        assert_eq!(remove_host("a.example", 22).unwrap(), 1);

        assert_eq!(
            check("a.example", 22, "ssh-ed25519", ED_KEY).unwrap(),
            KeyStatus::Revoked,
        );
    }

    /// The rewrite goes through a temp file beside the real one; none may
    /// be left behind.
    #[test]
    fn removing_a_host_leaves_no_temp_file() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts(&format!("a.example ssh-ed25519 {ED_KEY}\n"));

        remove_host("a.example", 22).unwrap();

        let mut names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["known_hosts", "known_hosts.lock"]);
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

    // -- OpenSSH line forms -------------------------------------------------

    #[test]
    fn a_revoked_key_is_refused_even_with_a_trusting_line() {
        let raw = format!(
            "prod.example.com ssh-ed25519 {ED_KEY}\n\
             @revoked * ssh-ed25519 {ED_KEY}\n"
        );
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Revoked);
    }

    #[test]
    fn a_revoked_key_is_refused_for_every_host() {
        // Revocation means the key is compromised; it vouches for nothing.
        let raw = format!("@revoked other.example ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "new.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Revoked);
    }

    #[test]
    fn revoking_one_key_leaves_the_others_alone() {
        let raw = format!(
            "prod.example.com ssh-ed25519 {ED_KEY}\n\
             @revoked * ssh-ed25519 {ED_KEY_2}\n"
        );
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn marker_lines_are_not_host_entries() {
        let raw = format!(
            "@cert-authority prod.example.com ssh-ed25519 {ED_KEY}\n\
             @revoked prod.example.com ssh-rsa {RSA_KEY}\n"
        );
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Unknown);
        assert!(stored_key_types_in_str(&raw, "prod.example.com", 22).is_empty());
    }

    #[test]
    fn a_comma_separated_host_list_matches_each_name() {
        let raw = format!("prod.example.com,10.0.0.5 ssh-ed25519 {ED_KEY}\n");
        for host in ["prod.example.com", "10.0.0.5"] {
            let r = check_in_str(&raw, host, 22, "ssh-ed25519", ED_KEY);
            assert_eq!(r, KeyStatus::Trusted, "{host}");
        }
        assert_eq!(
            stored_key_types_in_str(&raw, "10.0.0.5", 22),
            vec!["ssh-ed25519".to_string()]
        );
    }

    #[test]
    fn fields_may_be_separated_by_tabs_and_runs_of_spaces() {
        let raw = format!("prod.example.com\t ssh-ed25519   {ED_KEY} comment\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn a_negated_name_keeps_the_line_from_applying() {
        let raw = format!("prod.example.com,!prod.example.com ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "prod.example.com", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Unknown);
    }

    #[test]
    fn the_legacy_form_is_not_read_for_an_ipv6_host() {
        // `::1:22` could be host `::1` on port 22 in the legacy form, or
        // host `::1:22` on port 22 in the canonical one.
        let raw = format!("::1:22 ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "::1", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Unknown);
        let r = check_in_str(&raw, "::1:22", 22, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
        let raw = format!("[::1]:2222 ssh-ed25519 {ED_KEY}\n");
        let r = check_in_str(&raw, "::1", 2222, "ssh-ed25519", ED_KEY);
        assert_eq!(r, KeyStatus::Trusted);
    }

    #[test]
    fn remove_takes_one_name_out_of_a_host_list() {
        let raw = format!(
            "prod.example.com,10.0.0.5 ssh-ed25519 {ED_KEY}\n\
             @revoked prod.example.com ssh-rsa {RSA_KEY}\n"
        );
        let (out, n) = filter_out_host(&raw, "prod.example.com", 22);
        assert_eq!(n, 1);
        assert_eq!(
            out,
            format!(
                "10.0.0.5 ssh-ed25519 {ED_KEY}\n\
                 @revoked prod.example.com ssh-rsa {RSA_KEY}\n"
            ),
            "the other name stays, and a revocation is never removed"
        );
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

    /// Created umask-default, the file was private only while its folder
    /// was; it's made 0600 like every other file blink writes.
    #[cfg(unix)]
    #[test]
    fn a_new_known_hosts_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let _home = crate::paths::test_home();
        super::append("a.example", 22, "ssh-ed25519", "AAAA").unwrap();
        let mode = std::fs::metadata(known_hosts_path().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

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

    /// The parser splits fields on any whitespace and host lists on commas,
    /// so a host holding either would be written as a line that reads back
    /// as something else.
    #[test]
    fn append_rejects_a_tab_or_comma_in_host() {
        let _home = crate::paths::test_home();
        for host in ["evil\thost", "a.example,b.example"] {
            let r = super::append(host, 22, "ssh-ed25519", "KEY");
            assert!(r.is_err(), "{host:?}");
        }
    }

    #[test]
    fn append_rejects_whitespace_in_the_key_fields() {
        let _home = crate::paths::test_home();
        assert!(super::append("h.example", 22, "ssh\ted25519", "KEY").is_err());
        assert!(super::append("h.example", 22, "ssh-ed25519", "KEY extra").is_err());
        assert!(super::append("h.example", 22, "ssh-ed25519", "").is_err());
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

    // -- hashed names ---------------------------------------------------------

    /// Written by OpenSSH's own `ssh-keygen -H` from
    /// `a.example ssh-ed25519 {ED_KEY}` and `[b.example]:2222 …`.
    const HASHED_A: &str = "|1|/vFRHUfW7wbP7gG4mC1gqEqH2io=|HJWwpCf91ODgEV7dMem2j/RtbcM=";
    const HASHED_B_2222: &str = "|1|avZEsNwhTxngzGxBsmVcRUNXiqw=|lZfCM6NBzwTKbyRaamb+QyOp21I=";

    /// A file from `HashKnownHosts=yes` (Debian and Ubuntu's default)
    /// matched nothing, so every host in it was asked about as new — and a
    /// man in the middle got the "accept?" prompt instead of a refusal.
    #[test]
    fn hashed_names_from_openssh_match() {
        let raw =
            format!("{HASHED_A} ssh-ed25519 {ED_KEY}\n{HASHED_B_2222} ssh-ed25519 {ED_KEY}\n");
        let check = |h, p, k| check_in_str(&raw, h, p, "ssh-ed25519", k);
        assert_eq!(check("a.example", 22, ED_KEY), KeyStatus::Trusted);
        assert_eq!(check("A.Example", 22, ED_KEY), KeyStatus::Trusted);
        assert_eq!(check("b.example", 2222, ED_KEY), KeyStatus::Trusted);
        assert!(matches!(
            check("a.example", 22, ED_KEY_2),
            KeyStatus::Changed { .. }
        ));
        assert_eq!(check("b.example", 22, ED_KEY), KeyStatus::Unknown);
        assert_eq!(check("c.example", 22, ED_KEY), KeyStatus::Unknown);
    }

    #[test]
    fn a_malformed_hashed_name_matches_nothing() {
        for name in [
            "|1|",
            "|1|nobar",
            "|1|!!!|???",
            "|2|/vFRHUfW7wbP7gG4mC1gqEqH2io=|x=",
        ] {
            let raw = format!("{name} ssh-ed25519 {ED_KEY}\n");
            assert_eq!(
                check_in_str(&raw, "a.example", 22, "ssh-ed25519", ED_KEY),
                KeyStatus::Unknown,
                "{name}"
            );
        }
    }

    #[test]
    fn removing_a_host_takes_out_its_hashed_names() {
        let raw = format!("{HASHED_A} ssh-ed25519 {ED_KEY}\nother ssh-ed25519 {ED_KEY}\n");
        let (out, n) = filter_out_host(&raw, "a.example", 22);
        assert_eq!(n, 1);
        assert_eq!(out, format!("other ssh-ed25519 {ED_KEY}\n"));
    }

    // -- wildcard patterns ----------------------------------------------------

    #[test]
    fn glob_matching() {
        for (p, t) in [
            ("*", ""),
            ("*", "anything"),
            ("a*", "a"),
            ("*.example.com", "a.example.com"),
            ("*.EXAMPLE.com", "a.b.example.com"),
            ("a*b*c", "axxbyyc"),
            ("a*b*c", "abcbc"),
            ("h?st", "host"),
            ("[*.ex]:2222", "[a.ex]:2222"),
        ] {
            assert!(glob_matches(p, t), "{p} {t}");
        }
        for (p, t) in [
            ("*.example.com", "example.com"),
            ("h?st", "hst"),
            ("a*b", "ac"),
            ("a*b*c", "axxbyy"),
            ("[*.ex]:2222", "[a.ex]:22"),
        ] {
            assert!(!glob_matches(p, t), "{p} {t}");
        }
    }

    #[test]
    fn wildcard_patterns_vouch_for_the_hosts_they_name() {
        let raw = format!(
            "*.example.com,!bad.example.com ssh-ed25519 {ED_KEY}\n\
             [*.other]:2222 ssh-ed25519 {ED_KEY}\n"
        );
        let check = |h, p, k| check_in_str(&raw, h, p, "ssh-ed25519", k);
        assert_eq!(check("a.example.com", 22, ED_KEY), KeyStatus::Trusted);
        assert!(matches!(
            check("a.example.com", 22, ED_KEY_2),
            KeyStatus::Changed { .. }
        ));
        assert_eq!(check("bad.example.com", 22, ED_KEY), KeyStatus::Unknown);
        assert_eq!(check("example.com", 22, ED_KEY), KeyStatus::Unknown);
        assert_eq!(check("x.other", 2222, ED_KEY), KeyStatus::Trusted);
        assert_eq!(check("x.other", 22, ED_KEY), KeyStatus::Unknown);
        assert_eq!(
            stored_key_types_in_str(&raw, "z.example.com", 22),
            ["ssh-ed25519"]
        );
    }

    /// Forgetting one host must not forget every host a pattern covers.
    #[test]
    fn removing_a_host_leaves_patterns_alone() {
        let raw = format!("*.example.com ssh-ed25519 {ED_KEY}\n");
        let (out, n) = filter_out_host(&raw, "a.example.com", 22);
        assert_eq!((out.as_str(), n), (raw.as_str(), 0));
    }

    // -- reading ------------------------------------------------------------

    /// One Latin-1 byte in a comment failed the whole read, and with it
    /// every SSH connection.
    #[test]
    fn a_file_that_isnt_utf8_is_still_read() {
        let _home = crate::paths::test_home();
        let mut bytes = b"# caf\xe9 server\n".to_vec();
        bytes.extend_from_slice(format!("a.example ssh-ed25519 {ED_KEY}\n").as_bytes());
        fs::write(known_hosts_path().unwrap(), bytes).unwrap();

        assert_eq!(
            check("a.example", 22, "ssh-ed25519", ED_KEY).unwrap(),
            KeyStatus::Trusted
        );
    }

    #[test]
    fn a_read_error_names_the_file() {
        let _home = crate::paths::test_home();
        let path = known_hosts_path().unwrap();
        fs::create_dir(&path).unwrap();

        let e = check("a.example", 22, "ssh-ed25519", ED_KEY)
            .unwrap_err()
            .to_string();
        assert!(e.contains(&path.display().to_string()), "{e}");
    }

    /// Removing rewrote every line ending as `\n`.
    #[test]
    fn removing_a_host_keeps_crlf_line_endings() {
        let raw = format!(
            "# kept\r\na.example ssh-ed25519 {ED_KEY}\r\nb.example,a.example ssh-rsa {RSA_KEY}\r\nlast ssh-rsa {RSA_KEY}"
        );
        let (out, n) = filter_out_host(&raw, "a.example", 22);
        assert_eq!(n, 2);
        assert_eq!(
            out,
            format!("# kept\r\nb.example ssh-rsa {RSA_KEY}\r\nlast ssh-rsa {RSA_KEY}")
        );
    }

    // -- appending ----------------------------------------------------------

    /// The prompt can be open for minutes. If another blink saved a
    /// different key meanwhile, writing this one too left both trusted.
    #[test]
    fn append_refuses_when_the_host_has_another_key_by_now() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts(&format!("a.example ssh-ed25519 {ED_KEY_2}\n"));

        let saved = append("a.example", 22, "ssh-ed25519", ED_KEY).unwrap();

        assert!(matches!(saved, Saved::Conflict(KeyStatus::Changed { .. })));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("a.example ssh-ed25519 {ED_KEY_2}\n"),
            "nothing written"
        );
    }

    #[test]
    fn append_refuses_a_revoked_key_and_skips_a_trusted_one() {
        let _home = crate::paths::test_home();
        write_known_hosts(&format!(
            "@revoked * ssh-rsa {RSA_KEY}\na.example ssh-ed25519 {ED_KEY}\n"
        ));
        assert_eq!(
            append("b.example", 22, "ssh-rsa", RSA_KEY).unwrap(),
            Saved::Conflict(KeyStatus::Revoked)
        );
        assert_eq!(
            append("a.example", 22, "ssh-ed25519", ED_KEY).unwrap(),
            Saved::AlreadyTrusted
        );
        assert_eq!(
            append("c.example", 22, "ssh-ed25519", ED_KEY).unwrap(),
            Saved::Added
        );
    }

    /// Stored bare, these read back as a comment, a negation, a marker, a
    /// hashed name or a pattern: never as this host.
    #[test]
    fn append_refuses_hosts_the_format_cant_hold() {
        let _home = crate::paths::test_home();
        for host in ["#h", "!h", "@h", "|1|h", "a*b", "a?b"] {
            assert!(append(host, 22, "ssh-ed25519", ED_KEY).is_err(), "{host}");
        }
        assert!(!known_hosts_path().unwrap().exists());
    }

    // -- session trust --------------------------------------------------------

    #[test]
    fn session_trust_sanitizes_what_it_reports_as_changed() {
        let trust = SessionTrust::new();
        trust.trust("h", 22, "evil\x1b[2J", ED_KEY);
        match trust.check("h", 22, "ssh-ed25519", ED_KEY_2) {
            KeyStatus::Changed {
                stored_key_type, ..
            } => assert!(!stored_key_type.contains('\x1b'), "{stored_key_type:?}"),
            other => panic!("{other:?}"),
        }
    }

    /// Removing replaces the file, so an append locking `known_hosts`
    /// itself could write into the replaced file and be lost. Both now hold
    /// `known_hosts.lock`; with it unobtainable, neither changes anything.
    #[test]
    fn changes_go_through_the_store_lock() {
        let _home = crate::paths::test_home();
        let path = write_known_hosts(&format!("a.example ssh-ed25519 {ED_KEY}\n"));
        fs::create_dir(path.with_file_name("known_hosts.lock")).unwrap();

        let e = remove_host("a.example", 22).unwrap_err().to_string();
        assert!(e.contains("known_hosts.lock"), "{e}");
        assert!(append("b.example", 22, "ssh-ed25519", ED_KEY).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("a.example ssh-ed25519 {ED_KEY}\n")
        );
    }
}
