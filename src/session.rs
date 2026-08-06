//! Saved sessions: one `.ini` file per session in [`paths::sessions_dir`].
//!
//! ```ini
//! [session]
//! name = production
//! protocol = sftp
//! host = prod.example.com
//! port = 22
//! username = user
//! remote_dir = /var/www/html
//! local_dir = /home/me/dl/prod
//!
//! [auth]
//! method = key                ; password | key | agent
//! key_path = ~/.ssh/id_ed25519
//!
//! [transfer]
//! parallel_downloads = 4      ; overrides global setting
//!
//! [appearance]
//! theme = tokyo-night         ; overrides global setting
//! ```
//!
//! Passwords are NEVER persisted; they are prompted at connect time and held
//! only in memory.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ini::Ini;

use crate::config;
use crate::error::{BlinkError, Result};
use crate::paths;

/// Maximum session file size accepted on load (64 KiB).
const MAX_SESSION_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    Sftp,
    Scp,
    Ftp,
    Ftps,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::Scp => "scp",
            Self::Ftp => "ftp",
            Self::Ftps => "ftps",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            Self::Sftp | Self::Scp => 22,
            // FTPS defaults to 21, not the 990 you might expect: blink speaks
            // *explicit* FTPS (RFC 4217 `AUTH TLS` on a plaintext control
            // connection), which servers offer on the standard FTP port. Port
            // 990 is implicit FTPS, where TLS starts before any FTP command —
            // a mode blink does not implement, so defaulting to it made every
            // portless `ftps://` session hang until the connect timeout.
            Self::Ftp | Self::Ftps => 21,
        }
    }
}

impl FromStr for Protocol {
    type Err = BlinkError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sftp" => Ok(Self::Sftp),
            "scp" => Ok(Self::Scp),
            "ftp" => Ok(Self::Ftp),
            "ftps" => Ok(Self::Ftps),
            _ => Err(BlinkError::config(
                "protocol must be one of: sftp, scp, ftp, ftps",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    /// Password is prompted at connect time and not stored.
    Password,
    /// SSH key on disk (only meaningful for sftp/scp).
    Key { path: PathBuf },
    /// Use ssh-agent (only meaningful for sftp/scp).
    Agent,
}

impl AuthMethod {
    pub fn label(&self) -> String {
        match self {
            Self::Password => "password".to_string(),
            Self::Key { path } => format!("key: {}", path.display()),
            Self::Agent => "ssh-agent".to_string(),
        }
    }
}

/// Result of enumerating the sessions directory: the sessions that loaded,
/// plus a `"<file>: <reason>"` line for each one that didn't.
#[derive(Debug, Clone)]
pub struct SessionListing {
    pub sessions: Vec<Session>,
    /// Files present in the sessions dir that failed to load. Non-empty
    /// means the user has a session on disk they cannot see in the UI.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub remote_dir: String,
    pub local_dir: Option<PathBuf>,
    pub auth: AuthMethod,
    /// Per-session override of the global `parallel_downloads` setting.
    pub parallel_downloads: Option<u8>,
    /// Per-session theme override.
    pub theme: Option<String>,
    /// Skip TLS chain-of-trust validation when this is true.
    ///
    /// Consulted only by the FTPS transport; SFTP/SCP use the known-hosts
    /// file for host-key trust and do not use this flag. Defaults to false;
    /// the user has to opt in per session.
    ///
    /// When this is enabled, blink still verifies the cert's hostname (SAN /
    /// CN must match `host`) and the handshake signature, and it pins the
    /// leaf cert SHA-256 in [`cert_sha256`] on the first connect — so future
    /// connections to the same host must present the same cert. Disabling
    /// chain trust only removes the CA-authority requirement.
    pub accept_invalid_certs: bool,

    /// Pinned leaf-certificate SHA-256, hex-encoded (lowercase).
    ///
    /// Populated automatically on the first successful FTPS connect with
    /// [`accept_invalid_certs`] enabled. Subsequent connects to the same
    /// session require this exact certificate; if the server's cert hash
    /// differs, the connection is rejected.
    ///
    /// Has no effect when [`accept_invalid_certs`] is false (normal CA
    /// verification is used instead).
    pub cert_sha256: Option<String>,
}

impl Session {
    /// Build the on-disk filename for this session, sanitizing characters that
    /// are unsafe in filesystem paths.
    fn filename(&self) -> String {
        Self::name_to_filename(&self.name)
    }

    /// Map the session's logical name to its on-disk filename.
    ///
    /// Path-unsafe characters collapse to `_` for filesystem hygiene, so two
    /// distinct names ("my prod" and "my_prod") would otherwise produce
    /// identical sanitized stems and silently clobber each other on save.
    /// Append the first eight hex chars of `sha256(name)` to disambiguate;
    /// the suffix is derived from the raw name (no sanitisation), so it's
    /// stable per logical name across sanitisation collisions.
    fn name_to_filename(name: &str) -> String {
        use sha2::{Digest, Sha256};
        let safe: String = name
            .chars()
            .map(|c| match c {
                '\0' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
                c => c,
            })
            .collect();
        let hash = Sha256::digest(name.as_bytes());
        let mut suffix = String::with_capacity(8);
        for b in &hash[..4] {
            use std::fmt::Write as _;
            let _ = write!(&mut suffix, "{b:02x}");
        }
        format!("{safe}-{suffix}.ini")
    }

    /// Filename written by older blink versions (sanitized stem only, no
    /// hash suffix). Kept around so [`save`] can clean up the legacy file
    /// the first time a pre-existing session is re-saved.
    fn legacy_name_to_filename(name: &str) -> String {
        let safe: String = name
            .chars()
            .map(|c| match c {
                '\0' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
                c => c,
            })
            .collect();
        format!("{safe}.ini")
    }

    pub fn path(&self) -> Result<PathBuf> {
        Ok(paths::sessions_dir()?.join(self.filename()))
    }

    /// Check that this session satisfies every invariant [`Self::load_from`]
    /// enforces, so a saved session can always be read back.
    ///
    /// `save` calls this before writing anything. Without it the writers and
    /// the reader disagreed about what a valid session is: the edit-session
    /// form applies no validation of its own, so typing a *relative* path
    /// into the Local dir field wrote a file that `load_from` then rejected.
    /// The save reported success, and on the next launch `list_all` skipped
    /// the file — the session vanished from the selector with its `.ini`
    /// still on disk and no way to reach it from the UI. Enforcing the
    /// invariant here rather than in each form means a new writer cannot
    /// reintroduce that.
    ///
    /// The rules are deliberately the loader's rules and no stricter, with
    /// one addition: fields that would corrupt the INI itself (newlines,
    /// nulls) are rejected everywhere. A value carrying a newline can never
    /// survive a load anyway — the parser ends the value at the line break —
    /// so nothing that currently round-trips is refused here.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(BlinkError::config("session.name must not be empty"));
        }
        validate_network_field("name", &self.name)?;
        validate_network_field("host", &self.host)?;
        validate_network_field("username", &self.username)?;
        validate_network_field("remote_dir", &self.remote_dir)?;

        if let Some(local) = &self.local_dir {
            let raw = local.to_string_lossy();
            if !local.is_absolute() && !raw.starts_with("~/") && raw != "~" {
                return Err(BlinkError::config(
                    "session.local_dir must be an absolute path or start with ~/",
                ));
            }
        }

        if let AuthMethod::Key { path } = &self.auth
            && !path.is_absolute()
        {
            return Err(BlinkError::config(
                "auth.key_path must be an absolute path",
            ));
        }

        if let Some(theme) = &self.theme {
            config::validate_theme_name(theme)?;
        }

        if let Some(pin) = &self.cert_sha256
            && (pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(BlinkError::config(
                "tls.cert_sha256 must be 64 lowercase hex characters",
            ));
        }

        Ok(())
    }

    /// Serialize and write the session file atomically (write to `.tmp`, rename).
    ///
    /// Refuses to write a session that [`Self::validate`] rejects, so the
    /// file on disk is always one `load_from` can read back.
    pub fn save(&self) -> Result<()> {
        self.validate()?;

        let path = self.path()?;
        let tmp = path.with_extension("tmp");

        let mut ini = Ini::new();
        ini.with_section(Some("session"))
            .set("name", &self.name)
            .set("protocol", self.protocol.as_str())
            .set("host", &self.host)
            .set("port", self.port.to_string())
            .set("username", &self.username)
            .set("remote_dir", &self.remote_dir);
        if let Some(local) = &self.local_dir {
            ini.with_section(Some("session"))
                .set("local_dir", local.display().to_string());
        }

        {
            let mut auth = ini.with_section(Some("auth"));
            match &self.auth {
                AuthMethod::Password => {
                    auth.set("method", "password");
                }
                AuthMethod::Key { path } => {
                    auth.set("method", "key")
                        .set("key_path", path.display().to_string());
                }
                AuthMethod::Agent => {
                    auth.set("method", "agent");
                }
            }
        }

        if let Some(p) = self.parallel_downloads {
            ini.with_section(Some("transfer"))
                .set("parallel_downloads", p.to_string());
        }
        if let Some(theme) = &self.theme {
            ini.with_section(Some("appearance")).set("theme", theme);
        }
        // Only persist a [tls] section when the user has opted in. Default
        // sessions don't get one, which keeps existing session files
        // unchanged on save.
        if self.accept_invalid_certs {
            let mut tls = ini.with_section(Some("tls"));
            tls.set("accept_invalid_certs", "true");
            if let Some(pin) = &self.cert_sha256 {
                tls.set("cert_sha256", pin);
            }
        }

        // Atomic + durable write: tempfile → sync_all → rename → fsync the
        // parent directory. Without the syncs, a power loss between rename
        // and the filesystem's journal commit can leave a zero-byte session
        // file or revert the rename entirely. See [`paths::sync_parent_dir`].
        {
            let mut f = fs::File::create(&tmp)?;
            ini.write_to(&mut f)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        paths::sync_parent_dir(&path)?;

        // Migration: older blink versions wrote at `<sanitized>.ini` with no
        // hash suffix. After a successful save under the new name, remove
        // the legacy file IF it exists AND it loads as a session with the
        // same logical name. We don't blow away an arbitrary file with the
        // same stem — only the one we previously wrote ourselves.
        let legacy = paths::sessions_dir()?.join(Self::legacy_name_to_filename(&self.name));
        if legacy != path && legacy.exists() {
            match Self::load_from(&legacy) {
                Ok(prev) if prev.name == self.name => {
                    if let Err(e) = fs::remove_file(&legacy) {
                        tracing::warn!(
                            path = %legacy.display(),
                            "could not remove legacy session file: {e}",
                        );
                    }
                }
                _ => {
                    // Either the legacy file doesn't parse as a session or
                    // its name field doesn't match — not ours, leave it.
                }
            }
        }

        Ok(())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        // Enforce a size limit before reading.
        let file = fs::File::open(path)?;
        let mut raw = String::new();
        file.take(MAX_SESSION_BYTES + 1).read_to_string(&mut raw)?;
        if raw.len() as u64 > MAX_SESSION_BYTES {
            return Err(BlinkError::config(format!(
                "session file is too large (limit is {MAX_SESSION_BYTES} bytes)"
            )));
        }

        let ini = Ini::load_from_str(&raw)
            .map_err(|e| BlinkError::config(format!("{}: {e}", path.display())))?;

        let s = ini.section(Some("session")).ok_or_else(|| {
            BlinkError::config(format!("{}: missing [session] section", path.display()))
        })?;

        let name = s
            .get("name")
            .ok_or_else(|| BlinkError::config("missing session.name"))?
            .to_string();
        let protocol: Protocol = s
            .get("protocol")
            .ok_or_else(|| BlinkError::config("missing session.protocol"))?
            .parse()?;

        let host = s
            .get("host")
            .ok_or_else(|| BlinkError::config("missing session.host"))?
            .to_string();
        validate_network_field("host", &host)?;

        let port: u16 = s
            .get("port")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| protocol.default_port());

        let username = s.get("username").unwrap_or("").to_string();
        validate_network_field("username", &username)?;

        let remote_dir = s.get("remote_dir").unwrap_or("/").to_string();

        let local_dir = match s.get("local_dir") {
            Some(v) => {
                let p = PathBuf::from(v);
                // Accept absolute paths and `~/`-prefixed paths (expanded at
                // use time by resolve_local_dir). Reject relative paths that
                // would resolve against the unpredictable process CWD.
                if !p.is_absolute() && !v.starts_with("~/") && v != "~" {
                    return Err(BlinkError::config(
                        "session.local_dir must be an absolute path or start with ~/",
                    ));
                }
                Some(p)
            }
            None => None,
        };

        let auth = match ini.section(Some("auth")) {
            Some(a) => match a.get("method").unwrap_or("password").trim() {
                "password" => AuthMethod::Password,
                "key" => {
                    let key_path = a.get("key_path").ok_or_else(|| {
                        BlinkError::config("auth.method=key but auth.key_path missing")
                    })?;
                    let p = PathBuf::from(key_path);
                    if !p.is_absolute() {
                        return Err(BlinkError::config(
                            "auth.key_path must be an absolute path",
                        ));
                    }
                    AuthMethod::Key { path: p }
                }
                "agent" => AuthMethod::Agent,
                _ => {
                    return Err(BlinkError::config(
                        "auth.method must be one of: password, key, agent",
                    ))
                }
            },
            None => AuthMethod::Password,
        };

        let parallel_downloads = ini
            .section(Some("transfer"))
            .and_then(|s| s.get("parallel_downloads"))
            .and_then(|v| v.parse().ok());

        let theme = match ini
            .section(Some("appearance"))
            .and_then(|s| s.get("theme"))
        {
            Some(v) => {
                config::validate_theme_name(v)?;
                Some(v.to_string())
            }
            None => None,
        };

        let tls_section = ini.section(Some("tls"));
        let accept_invalid_certs = tls_section
            .and_then(|s| s.get("accept_invalid_certs"))
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        let cert_sha256 = tls_section
            .and_then(|s| s.get("cert_sha256"))
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| {
                // Reject anything but lowercase hex of length 64. Be strict —
                // a malformed pin would either silently accept the wrong cert
                // (uppercase mismatch in eq_ignore_case is fine, but garbage
                // characters would slip through). Normalize to lowercase.
                let lower = v.to_ascii_lowercase();
                if lower.len() == 64 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Ok(lower)
                } else {
                    Err(BlinkError::config(
                        "tls.cert_sha256 must be 64 lowercase hex characters",
                    ))
                }
            })
            .transpose()?;

        Ok(Self {
            name,
            protocol,
            host,
            port,
            username,
            remote_dir,
            local_dir,
            auth,
            parallel_downloads,
            theme,
            accept_invalid_certs,
            cert_sha256,
        })
    }

    /// List all saved sessions, sorted by name. Files that fail to load are
    /// skipped rather than aborting the listing; use [`Self::list_all_detailed`]
    /// when the caller can tell the user about them.
    pub fn list_all() -> Result<Vec<Self>> {
        Ok(Self::list_all_detailed()?.sessions)
    }

    /// Like [`Self::list_all`], but also reports the files that were skipped.
    ///
    /// A skipped file is invisible to the user — the session simply isn't in
    /// the selector — and the `tracing::warn` this used to emit goes nowhere
    /// unless `BLINK_LOG_FILE` is set, because `init_tracing` sends logs to a
    /// sink otherwise. Handing the reasons back lets the TUI and the CLI say
    /// which file was dropped and why, instead of leaving the user to wonder
    /// where their session went.
    pub fn list_all_detailed() -> Result<SessionListing> {
        let dir = paths::sessions_dir()?;
        let mut sessions = Vec::new();
        let mut skipped = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ini") {
                continue;
            }
            match Self::load_from(&path) {
                Ok(s) => sessions.push(s),
                Err(e) => {
                    tracing::warn!(?path, "skipping bad session: {e}");
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    skipped.push(format!("{name}: {e}"));
                }
            }
        }
        sessions.sort_by(|a, b| a.name.cmp(&b.name));
        skipped.sort();
        Ok(SessionListing { sessions, skipped })
    }

    pub fn delete(name: &str) -> Result<()> {
        let dir = paths::sessions_dir()?;

        // Fast path: try the expected filename directly. This is O(1) for the
        // common case where the name maps uniquely to its sanitized filename.
        let candidate = dir.join(Self::name_to_filename(name));
        if let Ok(s) = Self::load_from(&candidate)
            && s.name == name {
                fs::remove_file(&candidate)?;
                return Ok(());
            }

        // Fallback scan: needed when two distinct names produce the same
        // sanitized filename (e.g. "my session" and "my_session").
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path == candidate {
                continue; // already tried above
            }
            if path.extension().and_then(|s| s.to_str()) != Some("ini") {
                continue;
            }
            if let Ok(s) = Self::load_from(&path)
                && s.name == name {
                    fs::remove_file(&path)?;
                    return Ok(());
                }
        }
        Err(BlinkError::session_not_found(name))
    }

    /// Build an ad-hoc session from a URL like `sftp://user@host:22/remote`.
    ///
    /// - protocol is required (`sftp` / `scp` / `ftp` / `ftps`)
    /// - user is optional (defaults to empty)
    /// - port is optional (defaults to the protocol's standard port)
    /// - path is optional (defaults to `/`)
    ///
    /// Auth defaults to [`AuthMethod::Password`] — the password is prompted at
    /// connect time. The session is not persisted; call [`save`] to do that.
    pub fn from_url(url: &str) -> Result<Self> {
        let s = url.trim();
        if s.is_empty() {
            return Err(BlinkError::config("empty URL"));
        }

        let (proto_str, rest) = s
            .split_once("://")
            .ok_or_else(|| BlinkError::config("missing `://` (try sftp://user@host)"))?;
        let protocol: Protocol = proto_str.parse()?;

        // Split off the path (everything from the first '/') so the authority
        // portion contains only `[user@]host[:port]`. Decode the path here so
        // names like `path%20with%20spaces` survive into the remote_dir.
        let (authority, remote_dir) = match rest.find('/') {
            Some(i) => (&rest[..i], percent_decode(&rest[i..], "remote path")?),
            None => (rest, "/".to_string()),
        };

        let (username, hostport) = match authority.split_once('@') {
            Some((u, h)) => (u.to_string(), h),
            None => (String::new(), authority),
        };

        // RFC 3986 §3.2.1 deprecates `user:password@` userinfo: passwords in
        // URLs end up in shell history, process listings, and well-meaning
        // tooling that prints "what you typed" without redacting. Reject the
        // form outright and point the user at the interactive prompt.
        if username.contains(':') {
            return Err(BlinkError::config(
                "password in URL is not supported — drop the `:password` and \
                 you'll be prompted for it at connect time",
            ));
        }

        // Percent-decode the username AFTER the password check so the check
        // sees the raw colon (an encoded `%3A` would still be a password).
        let username = percent_decode(&username, "username")?;

        let (host, port) = if hostport.starts_with('[') {
            // Bracketed IPv6 literal: [::1] or [::1]:22
            let close = hostport
                .find(']')
                .ok_or_else(|| BlinkError::config("unclosed '[' in host — IPv6 addresses must use [::1]:port notation"))?;
            let ip = &hostport[1..close];
            let after = &hostport[close + 1..];
            let port = if after.is_empty() {
                protocol.default_port()
            } else if let Some(port_str) = after.strip_prefix(':') {
                port_str
                    .parse::<u16>()
                    .map_err(|_| BlinkError::config(format!("bad port: {port_str}")))?
            } else {
                return Err(BlinkError::config(format!(
                    "unexpected text after ']': {after}"
                )));
            };
            (ip.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => {
                    // A colon inside `h` means this is a bare IPv6 address,
                    // which is ambiguous without brackets — reject it clearly.
                    if h.contains(':') {
                        return Err(BlinkError::config(
                            "bare IPv6 addresses are not valid in URLs — use [::1]:port notation",
                        ));
                    }
                    let parsed = p
                        .parse::<u16>()
                        .map_err(|_| BlinkError::config(format!("bad port: {p}")))?;
                    (h.to_string(), parsed)
                }
                None => (hostport.to_string(), protocol.default_port()),
            }
        };

        if host.is_empty() {
            return Err(BlinkError::config("missing host"));
        }

        validate_network_field("host", &host)?;
        validate_network_field("username", &username)?;

        Ok(Self {
            name: host.clone(),
            protocol,
            host,
            port,
            username,
            remote_dir,
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        })
    }
}

/// Decode `%xx` percent-escapes in a URL component into a UTF-8 string.
///
/// Rejects malformed escapes (`%`, `%X`, `%XY` where X/Y aren't hex) and
/// escapes that don't form valid UTF-8 — silently passing through invalid
/// bytes would let a malicious URL inject names that look different
/// rendered in the TUI than they do as bytes on the wire.
fn percent_decode(s: &str, field: &str) -> Result<String> {
    // Common case: no `%`. Skip the byte walk.
    if !s.contains('%') {
        return Ok(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 2 >= bytes.len() {
            return Err(BlinkError::config(format!(
                "incomplete percent-escape in {field}"
            )));
        }
        let hi = hex_digit(bytes[i + 1]).ok_or_else(|| {
            BlinkError::config(format!("invalid percent-escape in {field}"))
        })?;
        let lo = hex_digit(bytes[i + 2]).ok_or_else(|| {
            BlinkError::config(format!("invalid percent-escape in {field}"))
        })?;
        out.push((hi << 4) | lo);
        i += 3;
    }
    String::from_utf8(out).map_err(|_| {
        BlinkError::config(format!("percent-escape decodes to invalid UTF-8 in {field}"))
    })
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Validate a field that is passed to the network/transport layer.
///
/// Null bytes cause hostname truncation in C-based SSH libraries, potentially
/// causing blink to connect to a different host than displayed. Newlines and
/// carriage returns could inject extra lines into known_hosts or log output.
fn validate_network_field(field: &str, value: &str) -> Result<()> {
    if value.bytes().any(|b| matches!(b, b'\0' | b'\n' | b'\r')) {
        return Err(BlinkError::config(format!(
            "session.{field} must not contain null bytes or newlines"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // validate_network_field
    #[test]
    fn network_field_clean_passes() {
        assert!(validate_network_field("host", "example.com").is_ok());
    }

    #[test]
    fn network_field_null_byte_rejected() {
        assert!(validate_network_field("host", "evil\0host").is_err());
    }

    #[test]
    fn network_field_newline_rejected() {
        assert!(validate_network_field("host", "host\ninjection").is_err());
    }

    #[test]
    fn network_field_carriage_return_rejected() {
        assert!(validate_network_field("username", "user\rname").is_err());
    }

    // from_url
    #[test]
    fn from_url_sftp_full() {
        let s = Session::from_url("sftp://bob@example.com:2222/var/www").unwrap();
        assert_eq!(s.protocol, Protocol::Sftp);
        assert_eq!(s.host, "example.com");
        assert_eq!(s.port, 2222);
        assert_eq!(s.username, "bob");
        assert_eq!(s.remote_dir, "/var/www");
    }

    #[test]
    fn from_url_default_port_sftp() {
        let s = Session::from_url("sftp://host.example.com").unwrap();
        assert_eq!(s.port, 22);
    }

    #[test]
    fn from_url_ftp_default_port() {
        let s = Session::from_url("ftp://files.example.com").unwrap();
        assert_eq!(s.port, 21);
        assert_eq!(s.protocol, Protocol::Ftp);
    }

    #[test]
    fn from_url_ftps_defaults_to_explicit_port() {
        // blink implements explicit FTPS (AUTH TLS), which lives on 21.
        // Defaulting to the implicit-FTPS port 990 would hang every
        // portless ftps:// session until the connect timeout.
        let s = Session::from_url("ftps://files.example.com").unwrap();
        assert_eq!(s.port, 21);
        assert_eq!(s.protocol, Protocol::Ftps);
    }

    #[test]
    fn ftps_default_port_is_not_implicit() {
        assert_eq!(Protocol::Ftps.default_port(), 21);
        assert_ne!(Protocol::Ftps.default_port(), 990);
    }

    #[test]
    fn from_url_explicit_port_still_honoured() {
        // A user with an implicit-mode server can still name 990 explicitly;
        // the default just no longer picks it for them.
        let s = Session::from_url("ftps://files.example.com:990").unwrap();
        assert_eq!(s.port, 990);
    }

    #[test]
    fn from_url_missing_scheme_errors() {
        assert!(Session::from_url("example.com").is_err());
    }

    #[test]
    fn from_url_unknown_protocol_errors() {
        assert!(Session::from_url("ssh://example.com").is_err());
    }

    #[test]
    fn from_url_empty_host_errors() {
        assert!(Session::from_url("sftp://").is_err());
    }

    #[test]
    fn from_url_ipv6_bracketed() {
        let s = Session::from_url("sftp://user@[::1]:2022/data").unwrap();
        assert_eq!(s.host, "::1");
        assert_eq!(s.port, 2022);
    }

    #[test]
    fn from_url_bare_ipv6_errors() {
        assert!(Session::from_url("sftp://::1/data").is_err());
    }

    #[test]
    fn from_url_null_in_host_rejected() {
        assert!(Session::from_url("sftp://evil\x00host/").is_err());
    }

    #[test]
    fn from_url_rejects_password_in_url() {
        let err = Session::from_url("sftp://alice:hunter2@host/").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("password") || msg.contains("prompt"),
            "error should explain password is not allowed: {msg}"
        );
    }

    #[test]
    fn from_url_percent_decodes_path() {
        let s = Session::from_url("sftp://host/var/www%20html").unwrap();
        assert_eq!(s.remote_dir, "/var/www html");
    }

    #[test]
    fn from_url_percent_decodes_username() {
        // Common case: encoded `@` so the literal `alice@corp` becomes the user
        // without splitting the authority twice.
        let s = Session::from_url("sftp://alice%40corp@host/").unwrap();
        assert_eq!(s.username, "alice@corp");
    }

    #[test]
    fn from_url_rejects_invalid_percent_escape() {
        assert!(Session::from_url("sftp://host/path%XYZ").is_err());
        assert!(Session::from_url("sftp://host/path%").is_err());
        assert!(Session::from_url("sftp://host/path%2").is_err());
    }

    #[test]
    fn from_url_rejects_percent_escape_to_invalid_utf8() {
        // 0xC3 0x28 is invalid UTF-8 (start of multi-byte but bad continuation).
        assert!(Session::from_url("sftp://host/%C3%28").is_err());
    }

    #[test]
    fn from_url_encoded_colon_decodes_into_username() {
        // The password-in-URL check runs against the literal bytes before
        // percent-decoding. An encoded `%3A` makes it into the username
        // verbatim. Safe: the colon is just an ordinary character in the
        // SSH username field; the server treats the whole string as the
        // user identity and either accepts it or returns auth-failed.
        let s = Session::from_url("sftp://alice%3Aworld@host/").unwrap();
        assert_eq!(s.username, "alice:world");
    }

    #[test]
    fn name_to_filename_disambiguates_sanitised_collisions() {
        // "my prod" and "my_prod" sanitise to the same stem; the hash
        // suffix must distinguish them so two saves don't clobber each
        // other.
        let a = Session::name_to_filename("my prod");
        let b = Session::name_to_filename("my_prod");
        assert_ne!(a, b, "{a} vs {b}");
        assert!(a.starts_with("my_prod-"));
        assert!(b.starts_with("my_prod-"));
    }

    #[test]
    fn name_to_filename_stable_per_logical_name() {
        // Hash is derived from the raw name (pre-sanitisation), so the
        // SAME logical name always produces the SAME filename.
        let a = Session::name_to_filename("production");
        let b = Session::name_to_filename("production");
        assert_eq!(a, b);
    }

    // -- validate ----------------------------------------------------------
    //
    // `save` calls this, so anything rejected here can never reach disk and
    // become a session that `load_from` refuses to read back.

    fn valid() -> Session {
        Session {
            name: "prod".into(),
            protocol: Protocol::Sftp,
            host: "prod.example.com".into(),
            port: 22,
            username: "me".into(),
            remote_dir: "/var/www".into(),
            local_dir: None,
            auth: AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        }
    }

    #[test]
    fn validate_accepts_a_plain_session() {
        assert!(valid().validate().is_ok());
    }

    #[test]
    fn validate_rejects_relative_local_dir() {
        // The regression this exists for: the edit-session form applies no
        // validation, so typing a relative path here used to save fine and
        // then make the session unloadable — it silently vanished from the
        // selector on the next launch.
        for bad in ["downloads", "./dl", "../shared"] {
            let mut s = valid();
            s.local_dir = Some(PathBuf::from(bad));
            let err = s.validate().unwrap_err().to_string();
            assert!(
                err.contains("local_dir"),
                "expected {bad:?} to be refused, got: {err}"
            );
        }
    }

    #[test]
    fn validate_accepts_absolute_and_tilde_local_dir() {
        for ok in ["/home/me/dl", "~/dl", "~"] {
            let mut s = valid();
            s.local_dir = Some(PathBuf::from(ok));
            assert!(s.validate().is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn validate_rejects_relative_key_path() {
        let mut s = valid();
        s.auth = AuthMethod::Key { path: PathBuf::from("id_ed25519") };
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_name() {
        let mut s = valid();
        s.name = "   ".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_ini_breaking_fields() {
        // A newline ends the value when the INI is parsed back, so a field
        // carrying one can never round-trip.
        for mutate in [
            (|s: &mut Session| s.host = "a\nb".into()) as fn(&mut Session),
            |s: &mut Session| s.username = "a\nb".into(),
            |s: &mut Session| s.remote_dir = "/a\nb".into(),
            |s: &mut Session| s.name = "a\nb".into(),
        ] {
            let mut s = valid();
            mutate(&mut s);
            assert!(s.validate().is_err(), "newline field must be refused");
        }
    }

    #[test]
    fn validate_rejects_bad_cert_pin() {
        let mut s = valid();
        s.cert_sha256 = Some("nothex".into());
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_traversing_theme_name() {
        let mut s = valid();
        s.theme = Some("../../etc/passwd".into());
        assert!(s.validate().is_err());
    }

    /// Everything `validate` accepts must survive a round-trip through the
    /// INI writer and `load_from`. This is the invariant the two sides were
    /// disagreeing about; assert it directly rather than per-field.
    #[test]
    fn validated_sessions_round_trip_through_the_loader() {
        let mut s = valid();
        s.local_dir = Some(PathBuf::from("/home/me/dl"));
        s.remote_dir = "/var/www/html".into();
        s.parallel_downloads = Some(4);
        s.theme = Some("tokyo-night".into());
        s.validate().expect("fixture must be valid");

        // Serialise exactly as `save` does, then read it back.
        let mut ini = Ini::new();
        ini.with_section(Some("session"))
            .set("name", &s.name)
            .set("protocol", s.protocol.as_str())
            .set("host", &s.host)
            .set("port", s.port.to_string())
            .set("username", &s.username)
            .set("remote_dir", &s.remote_dir)
            .set("local_dir", s.local_dir.as_ref().unwrap().display().to_string());
        ini.with_section(Some("auth")).set("method", "password");
        ini.with_section(Some("transfer")).set("parallel_downloads", "4");
        ini.with_section(Some("appearance")).set("theme", "tokyo-night");

        let mut buf: Vec<u8> = Vec::new();
        ini.write_to(&mut buf).unwrap();
        let raw = String::from_utf8(buf).unwrap();

        let dir = std::env::temp_dir()
            .join(format!("blink-session-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.ini");
        std::fs::write(&path, raw).unwrap();

        let loaded = Session::load_from(&path).expect("validated session must load");
        assert_eq!(loaded.name, s.name);
        assert_eq!(loaded.host, s.host);
        assert_eq!(loaded.remote_dir, s.remote_dir);
        assert_eq!(loaded.local_dir, s.local_dir);
        assert_eq!(loaded.parallel_downloads, Some(4));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
