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
            Self::Ftp => 21,
            Self::Ftps => 990,
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
    /// Skip TLS certificate validation when this is true.
    ///
    /// Consulted only by the FTPS transport; SFTP/SCP use the known-hosts
    /// file for host-key trust and do not use this flag. Defaults to false;
    /// the user has to opt in per session.
    ///
    /// **Dangerous.** Disables the protections TLS is supposed to give you.
    /// The UI flags it in red.
    pub accept_invalid_certs: bool,
}

impl Session {
    /// Build the on-disk filename for this session, sanitizing characters that
    /// are unsafe in filesystem paths.
    fn filename(&self) -> String {
        Self::name_to_filename(&self.name)
    }

    fn name_to_filename(name: &str) -> String {
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

    /// Serialize and write the session file atomically (write to `.tmp`, rename).
    pub fn save(&self) -> Result<()> {
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
        // Only persist when the user has explicitly opted in. Default-false
        // sessions don't get a [tls] section at all, which keeps existing
        // session files unchanged on save.
        if self.accept_invalid_certs {
            ini.with_section(Some("tls"))
                .set("accept_invalid_certs", "true");
        }

        ini.write_to_file(&tmp)?;
        fs::rename(&tmp, &path)?;
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

        let accept_invalid_certs = ini
            .section(Some("tls"))
            .and_then(|s| s.get("accept_invalid_certs"))
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);

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
        })
    }

    /// List all saved sessions, sorted by name. Bad files are skipped with a
    /// warning rather than aborting the listing.
    pub fn list_all() -> Result<Vec<Self>> {
        let dir = paths::sessions_dir()?;
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ini") {
                continue;
            }
            let load = Self::load_from(&path);
            match load {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!(?path, "skipping bad session: {e}"),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    pub fn delete(name: &str) -> Result<()> {
        let dir = paths::sessions_dir()?;

        // Fast path: try the expected filename directly. This is O(1) for the
        // common case where the name maps uniquely to its sanitized filename.
        let candidate = dir.join(Self::name_to_filename(name));
        if let Ok(s) = Self::load_from(&candidate) {
            if s.name == name {
                fs::remove_file(&candidate)?;
                return Ok(());
            }
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
            if let Ok(s) = Self::load_from(&path) {
                if s.name == name {
                    fs::remove_file(&path)?;
                    return Ok(());
                }
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
        // portion contains only `[user@]host[:port]`.
        let (authority, remote_dir) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };

        let (username, hostport) = match authority.split_once('@') {
            Some((u, h)) => (u.to_string(), h),
            None => (String::new(), authority),
        };

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
        })
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
}
