//! One error enum used throughout blink.

use std::borrow::Cow;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// Maximum number of characters kept in any sanitized error string.
const MAX_ERR_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum BlinkError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("theme not found: {0}")]
    ThemeNotFound(String),

    #[allow(dead_code)]
    #[error("invalid path: {0}")]
    InvalidPath(PathBuf),

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("ssh key is encrypted; a passphrase is required")]
    KeyNeedsPassphrase,

    /// The server presented a host key that does not match the stored one.
    /// This is a hard error — possible man-in-the-middle attack.
    #[allow(dead_code)]
    #[error(
        "host key mismatch for {host}: expected {stored_key_type} key, got {presented_key_type}. \
         If the server key was legitimately changed, remove the old entry from the known_hosts file."
    )]
    HostKeyChanged {
        host: String,
        stored_key_type: String,
        presented_key_type: String,
    },

    /// The server's host key is not in the known-hosts file and the user
    /// rejected it (pressed `n` at the confirmation prompt).
    #[allow(dead_code)]
    #[error("host key rejected by user for {0}")]
    HostKeyRejected(String),

    #[error("connection failed: {0}")]
    Connect(String),

    /// Remote path doesn't exist. Distinct from `Transport(...)` so callers
    /// can short-circuit (e.g. metadata probes that want to return
    /// `Ok(None)`, or retry loops that should bail instead of backing off).
    #[error("not found: {0}")]
    NotFound(String),

    /// Remote rejected the operation for permission / policy reasons. SFTP
    /// `PermissionDenied` (status 3) and FTP `550 Permission denied` /
    /// `533 Action denied for policy reasons` end up here.
    #[error("permission denied: {0}")]
    Permission(String),

    /// The underlying connection is gone — TCP reset, TLS torn down,
    /// SSH session closed, FTP control channel dropped. Distinct from
    /// `Transport(...)` so a UI layer can render this as "lost
    /// connection" rather than a generic protocol error.
    #[error("connection lost: {0}")]
    Disconnected(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[allow(dead_code)]
    #[error("protocol `{0}` is not yet implemented")]
    NotImplemented(&'static str),
}

impl BlinkError {
    pub fn config<S: Into<String>>(msg: S) -> Self {
        Self::Config(sanitize(msg.into()))
    }
    pub fn transport<S: Into<String>>(msg: S) -> Self {
        Self::Transport(sanitize(msg.into()))
    }
    pub fn auth<S: Into<String>>(msg: S) -> Self {
        Self::AuthFailed(sanitize(msg.into()))
    }
    pub fn connect<S: Into<String>>(msg: S) -> Self {
        Self::Connect(sanitize(msg.into()))
    }
    pub fn not_found<S: Into<String>>(msg: S) -> Self {
        Self::NotFound(sanitize(msg.into()))
    }
    pub fn permission<S: Into<String>>(msg: S) -> Self {
        Self::Permission(sanitize(msg.into()))
    }
    pub fn disconnected<S: Into<String>>(msg: S) -> Self {
        Self::Disconnected(sanitize(msg.into()))
    }
    pub fn session_not_found<S: Into<String>>(name: S) -> Self {
        Self::SessionNotFound(sanitize(name.into()))
    }
    pub fn theme_not_found<S: Into<String>>(name: S) -> Self {
        Self::ThemeNotFound(sanitize(name.into()))
    }
    /// Construct a `HostKeyChanged` error with all fields sanitized.
    ///
    /// `presented_key_type` comes directly from the remote server and must be
    /// sanitized before it appears in an error message or the TUI.
    #[allow(dead_code)]
    pub fn host_key_changed(
        host: impl Into<String>,
        stored_key_type: impl Into<String>,
        presented_key_type: impl Into<String>,
    ) -> Self {
        Self::HostKeyChanged {
            host: sanitize(host.into()),
            stored_key_type: sanitize(stored_key_type.into()),
            presented_key_type: sanitize(presented_key_type.into()),
        }
    }
}

/// Characters that are invisible or that reorder the text around them.
///
/// `char::is_control` only covers Unicode category **Cc** (U+0000–U+001F,
/// U+007F–U+009F). The bidirectional-formatting and zero-width characters
/// below are category **Cf**, so they sail straight through it — and they
/// are the dangerous ones for a file manager, because remote filenames get
/// rendered next to their extensions:
///
/// - A RIGHT-TO-LEFT OVERRIDE lets a server name a file so the pane displays
///   `report.png` while the bytes actually end in `.exe` or `.sh`. Same trick
///   as "trojan source", pointed at the file listing instead of a compiler.
/// - Zero-width characters let two different names render identically, so a
///   listing can show what looks like one entry twice, or disguise a name the
///   user already cleared through the overwrite prompt.
///
/// Replacing them with a space (the same policy the control characters get)
/// is deliberate: it makes the difference *visible* rather than silently
/// collapsing two distinct names onto one rendering.
///
/// U+200D ZERO WIDTH JOINER is excluded on purpose. It joins rather than
/// hides or reorders, so it can't spoof an extension, and stripping it would
/// break legitimate emoji sequences in filenames into separate glyphs.
fn is_deceptive_format(ch: char) -> bool {
    matches!(ch,
        '\u{061C}'                  // ARABIC LETTER MARK
        | '\u{200B}'..='\u{200C}'   // ZERO WIDTH SPACE, NON-JOINER
        | '\u{200E}'..='\u{200F}'   // LEFT-TO-RIGHT / RIGHT-TO-LEFT MARK
        | '\u{202A}'..='\u{202E}'   // embeddings and overrides (LRO/RLO here)
        | '\u{2066}'..='\u{2069}'   // isolates (LRI, RLI, FSI, PDI)
        | '\u{FEFF}'                // ZERO WIDTH NO-BREAK SPACE (BOM)
    )
}

/// True for any character that must not reach the terminal verbatim: ANSI
/// escape starters and other control codes, plus the invisible/reordering
/// formatters described on [`is_deceptive_format`].
fn is_unsafe_for_display(ch: char) -> bool {
    ch.is_control() || is_deceptive_format(ch)
}

/// Strip characters that are unsafe to render and truncate to `MAX_ERR_CHARS`.
///
/// Prevents a malicious server from injecting terminal escape sequences, or
/// invisible / text-reordering characters, into strings the TUI renders.
/// Offending characters are replaced with a space; runs of resulting spaces
/// are left as-is so callers can see where content was stripped. Strings
/// exceeding the character limit are truncated with `…`.
pub(crate) fn sanitize(s: String) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_ERR_CHARS + 4));
    let mut truncated = false;

    for (count, ch) in s.chars().enumerate() {
        if count >= MAX_ERR_CHARS {
            truncated = true;
            break;
        }
        out.push(if is_unsafe_for_display(ch) { ' ' } else { ch });
    }

    if truncated {
        out.push('…');
    }
    out
}

/// Strip unsafe characters from a single line of file content for safe
/// terminal rendering in the text viewer.
///
/// Unlike [`sanitize`], no length cap is applied — line length is bounded by
/// the caller's `MAX_PREVIEW_BYTES` cap, and Ratatui clips to terminal width.
/// Tabs are preserved (terminals handle them correctly). Newlines/carriage
/// returns never appear here because the caller splits on `lines()` first.
///
/// The bidi filter matters as much here as it does for filenames: viewing a
/// remote source file is exactly the "trojan source" setting, where an
/// override can make a line of code read as something other than what it
/// says.
pub(crate) fn sanitize_line(s: &str) -> String {
    s.chars()
        .map(|ch| if ch == '\t' || !is_unsafe_for_display(ch) { ch } else { ' ' })
        .collect()
}

/// Strip unsafe characters from a user-supplied string before printing it
/// directly to the terminal (e.g. CLI warnings, session listings).
///
/// Returns the input unchanged (as `Cow::Borrowed`) when there is nothing to
/// strip, avoiding an allocation on the common case. Otherwise replaces the
/// offending characters with a space — same policy as [`sanitize`], so a name
/// like `host\x00name` becomes `host name` rather than silently collapsing to
/// `hostname`.
pub(crate) fn sanitize_display(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_unsafe_for_display) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(
            s.chars()
                .map(|c| if is_unsafe_for_display(c) { ' ' } else { c })
                .collect(),
        )
    }
}

pub type Result<T> = std::result::Result<T, BlinkError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clean_string_unchanged() {
        let s = "hello world".to_string();
        assert_eq!(sanitize(s), "hello world");
    }

    #[test]
    fn sanitize_replaces_control_chars() {
        let s = "hello\x1b[31mworld\x07".to_string();
        let out = sanitize(s);
        assert!(!out.contains('\x1b'), "ESC must be stripped");
        assert!(!out.contains('\x07'), "BEL must be stripped");
        assert!(out.contains("hello"), "printable chars kept");
        assert!(out.contains("world"), "printable chars kept");
    }

    #[test]
    fn sanitize_truncates_long_string() {
        let long = "a".repeat(MAX_ERR_CHARS + 100);
        let out = sanitize(long);
        assert!(out.ends_with('…'), "truncated string must end with ellipsis");
        assert!(out.chars().count() <= MAX_ERR_CHARS + 1);
    }

    #[test]
    fn sanitize_exact_limit_not_truncated() {
        let s = "b".repeat(MAX_ERR_CHARS);
        let out = sanitize(s.clone());
        assert_eq!(out, s);
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn sanitize_line_preserves_tabs() {
        let s = "col1\tcol2";
        assert_eq!(sanitize_line(s), "col1\tcol2");
    }

    #[test]
    fn sanitize_line_strips_control_not_tab() {
        let s = "a\x01b\tc";
        let out = sanitize_line(s);
        assert_eq!(out, "a b\tc");
    }

    #[test]
    fn sanitize_display_clean_borrows() {
        let out = sanitize_display("hello");
        assert_eq!(&*out, "hello");
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn sanitize_display_replaces_control_chars() {
        let out = sanitize_display("evil\x1b[31mred\x07");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert!(out.contains("evil"));
        assert!(out.contains("red"));
        assert!(matches!(out, Cow::Owned(_)));
    }

    #[test]
    fn sanitize_display_null_byte_replaced_with_space() {
        let out = sanitize_display("host\x00name");
        assert_eq!(&*out, "host name");
    }

    // -- Bidi / zero-width filtering ---------------------------------------
    //
    // These are category Cf, which `char::is_control` (category Cc) does not
    // cover — the gap that let a server spoof a filename's extension.

    #[test]
    fn sanitize_strips_rtl_override_extension_spoof() {
        // "report\u{202E}gnp.exe" renders as "reportexe.png" in a terminal
        // that honours the override: the user sees an image, gets a binary.
        let out = sanitize("report\u{202E}gnp.exe".to_string());
        assert!(!out.contains('\u{202E}'), "RLO must be stripped: {out:?}");
        assert_eq!(out, "report gnp.exe");
    }

    #[test]
    fn sanitize_strips_every_bidi_formatter() {
        for ch in [
            '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}',
            '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            let out = sanitize(format!("a{ch}b"));
            assert_eq!(out, "a b", "U+{:04X} must be neutralised", ch as u32);
        }
    }

    #[test]
    fn sanitize_strips_zero_width_characters() {
        for ch in ['\u{200B}', '\u{200C}', '\u{FEFF}'] {
            let out = sanitize(format!("a{ch}b"));
            assert_eq!(out, "a b", "U+{:04X} must be neutralised", ch as u32);
        }
    }

    #[test]
    fn sanitize_makes_aliased_names_visibly_different() {
        // The point of replacing with a space rather than deleting: two names
        // that rendered identically must not still render identically.
        let plain = sanitize("invoice.pdf".to_string());
        let hidden = sanitize("invoice\u{200B}.pdf".to_string());
        assert_ne!(plain, hidden);
    }

    #[test]
    fn sanitize_keeps_zero_width_joiner_for_emoji() {
        // ZWJ joins rather than hides or reorders, so it can't spoof an
        // extension — and stripping it would split emoji sequences apart.
        let family = "👨\u{200D}👩\u{200D}👧.png";
        assert_eq!(sanitize(family.to_string()), family);
    }

    #[test]
    fn sanitize_preserves_ordinary_non_ascii() {
        // The filter must not become "ASCII only" — plenty of legitimate
        // filenames are not.
        for s in ["café.txt", "日本語.md", "Ω-report.pdf", "naïve.rs"] {
            assert_eq!(sanitize(s.to_string()), s, "{s} must survive intact");
        }
    }

    #[test]
    fn sanitize_line_strips_bidi_in_viewed_source() {
        // Trojan-source: an override inside a viewed file can make a line of
        // code read as something other than what it does.
        let out = sanitize_line("let admin = false; // \u{202E}まsecret");
        assert!(!out.contains('\u{202E}'));
    }

    #[test]
    fn sanitize_display_strips_bidi_and_borrows_when_clean() {
        assert!(matches!(sanitize_display("plain.txt"), Cow::Borrowed(_)));
        let out = sanitize_display("evil\u{202E}txt.exe");
        assert!(!out.contains('\u{202E}'));
        assert!(matches!(out, Cow::Owned(_)));
    }
}
