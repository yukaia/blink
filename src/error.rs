//! One error enum used throughout blink.

use std::borrow::Cow;
use std::io;
use thiserror::Error;

/// Longest string [`sanitize`] returns, in characters. It caps every
/// message the TUI shows — errors, log lines, names — so one long remote
/// path can't fill the screen. Longer strings lose their middle, not their
/// end: that's where an error says what went wrong.
const MAX_SANITIZED_CHARS: usize = 512;

#[derive(Debug, Error)]
pub enum BlinkError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// A problem in a configuration file — `config.ini`, a session or a
    /// theme — or in a value bound for one.
    #[error("config error: {0}")]
    Config(String),

    /// A URL given to `blink connect` (or the new-session form) that
    /// can't be used.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// Any other local failure — known_hosts, checkpoints, the config
    /// directory itself. The message says what and where; no category is
    /// prefixed, since "config error" on an unreadable known_hosts sent
    /// people looking in config.ini.
    #[error("{0}")]
    Other(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("theme not found: {0}")]
    ThemeNotFound(String),

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("ssh key is encrypted; a passphrase is required")]
    KeyNeedsPassphrase,

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
}

impl BlinkError {
    pub fn config<S: Into<String>>(msg: S) -> Self {
        Self::Config(sanitize(msg.into()))
    }
    pub fn other<S: Into<String>>(msg: S) -> Self {
        Self::Other(sanitize(msg.into()))
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
/// The rest of the list is other characters that draw as nothing, or as a
/// blank, and so make the same aliasing possible: the remaining invisible
/// format characters (soft hyphen, word joiner, the invisible operators,
/// tags), and the Hangul fillers, which are letters but render empty.
/// The tag block (U+E0000–E007F) can spell out a whole hidden string.
///
/// U+200D ZERO WIDTH JOINER is excluded on purpose. It joins rather than
/// hides or reorders, so it can't spoof an extension, and stripping it would
/// break legitimate emoji sequences in filenames into separate glyphs.
/// Variation selectors are judged by what they follow; see
/// [`selector_allowed`].
fn is_deceptive_format(ch: char) -> bool {
    matches!(ch,
        '\u{00AD}'                  // SOFT HYPHEN
        | '\u{034F}'                // COMBINING GRAPHEME JOINER
        | '\u{061C}'                // ARABIC LETTER MARK
        | '\u{115F}'..='\u{1160}'   // HANGUL CHOSEONG / JUNGSEONG FILLER
        | '\u{17B4}'..='\u{17B5}'   // KHMER VOWEL INHERENT AQ / AA (draw as nothing)
        | '\u{180E}'                // MONGOLIAN VOWEL SEPARATOR
        | '\u{200B}'..='\u{200C}'   // ZERO WIDTH SPACE, NON-JOINER
        | '\u{200E}'..='\u{200F}'   // LEFT-TO-RIGHT / RIGHT-TO-LEFT MARK
        | '\u{2028}'..='\u{2029}'   // LINE / PARAGRAPH SEPARATOR (break layout)
        | '\u{202A}'..='\u{202E}'   // embeddings and overrides (LRO/RLO here)
        | '\u{2060}'..='\u{2064}'   // WORD JOINER, invisible operators
        | '\u{2066}'..='\u{2069}'   // isolates (LRI, RLI, FSI, PDI)
        | '\u{206A}'..='\u{206F}'   // deprecated formatting characters
        | '\u{2800}'                // BRAILLE PATTERN BLANK (the "invisible name" trick)
        | '\u{3164}'                // HANGUL FILLER
        | '\u{FEFF}'                // ZERO WIDTH NO-BREAK SPACE (BOM)
        | '\u{FFA0}'                // HALFWIDTH HANGUL FILLER
        | '\u{FFF9}'..='\u{FFFB}'   // interlinear annotation
        | '\u{1BCA0}'..='\u{1BCA3}' // shorthand format controls
        | '\u{1D173}'..='\u{1D17A}' // musical symbol formatting
        | '\u{E0000}'..='\u{E007F}' // tags
    )
}

/// True for any character that must not reach the terminal verbatim: ANSI
/// escape starters and other control codes, plus the invisible/reordering
/// formatters described on [`is_deceptive_format`]. Variation selectors
/// are decided in context by [`unsafe_at`].
fn is_unsafe_for_display(ch: char) -> bool {
    ch.is_control() || is_deceptive_format(ch)
}

fn is_variation_selector(ch: char) -> bool {
    matches!(ch, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Whether a variation selector `ch` after `prev` (and before `next`) does
/// its job: choosing how the character before it is drawn. Anywhere else it
/// draws as nothing, so `a` + U+FE0F looked exactly like `a` — the same
/// aliasing zero-width spaces are replaced for.
///
/// - U+E0100–E01EF pick an ideograph's form: only after a CJK ideograph.
/// - U+FE00–FE0F pick emoji or text presentation, and math or other
///   symbol variants: after any visible non-ASCII character. After ASCII,
///   only in a keycap: `1` U+FE0F U+20E3 is the 1️⃣ emoji.
fn selector_allowed(prev: Option<char>, ch: char, next: Option<char>) -> bool {
    let Some(p) = prev.filter(|&p| !is_unsafe_for_display(p) && !p.is_whitespace()) else {
        return false;
    };
    if ch >= '\u{E0100}' {
        return matches!(p,
            '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}' | '\u{20000}'..='\u{3FFFF}');
    }
    if p.is_ascii() {
        return matches!(p, '0'..='9' | '#' | '*') && next == Some('\u{20E3}');
    }
    true
}

/// Whether `chars[i]` must not reach the terminal, given its neighbours.
fn unsafe_at(chars: &[char], i: usize) -> bool {
    let ch = chars[i];
    if is_variation_selector(ch) {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        return !selector_allowed(prev, ch, chars.get(i + 1).copied());
    }
    is_unsafe_for_display(ch)
}

/// `s` with every unsafe character (other than those `keep` allows)
/// replaced by a space, or `None` if there was nothing to replace.
fn scrub(s: &str, keep: impl Fn(char) -> bool) -> Option<String> {
    // The common case, checked without allocating: nothing unsafe, and no
    // variation selector to look at in context.
    if !s
        .chars()
        .any(|c| (is_unsafe_for_display(c) && !keep(c)) || is_variation_selector(c))
    {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    let out: String = (0..chars.len())
        .map(|i| {
            if unsafe_at(&chars, i) && !keep(chars[i]) {
                ' '
            } else {
                chars[i]
            }
        })
        .collect();
    (out != s).then_some(out)
}

/// Strip characters that are unsafe to render, and cut the result to
/// [`MAX_SANITIZED_CHARS`].
///
/// Prevents a malicious server from injecting terminal escape sequences, or
/// invisible / text-reordering characters, into strings the TUI renders.
/// Offending characters are replaced with a space; runs of resulting spaces
/// are left as-is so callers can see where content was stripped.
///
/// A longer string keeps its start and its end with `…` between: keeping
/// only the start cut `upload failed: <long path>: permission denied` off
/// before the reason. Running it twice gives the same result.
pub(crate) fn sanitize(s: String) -> String {
    let s = scrub(&s, |_| false).unwrap_or(s);
    let count = s.chars().count();
    if count <= MAX_SANITIZED_CHARS {
        return s;
    }
    let head = (MAX_SANITIZED_CHARS - 1) / 2;
    let tail = MAX_SANITIZED_CHARS - 1 - head;
    let mut out: String = s.chars().take(head).collect();
    out.push('…');
    out.extend(s.chars().skip(count - tail));
    out
}

/// Tab stops in the text viewer: every 8 columns, as terminals, `less` and
/// `cat` place them, and as Go source and Makefiles assume.
const TAB_WIDTH: usize = 8;

/// Strip unsafe characters from a single line of file content for safe
/// terminal rendering in the text viewer.
///
/// Unlike [`sanitize`], no length cap is applied — line length is bounded by
/// the viewer's size limit, and Ratatui clips to terminal width.
/// Newlines/carriage returns never appear here because the caller splits on
/// `lines()` first.
///
/// Tabs are expanded to spaces up to the next [`TAB_WIDTH`] stop, counted
/// from the start of the line in terminal columns, so a double-width
/// character before a tab counts as the two columns it fills. Keeping tabs
/// was not an option: ratatui drops every control grapheme when it renders,
/// `\t` included, so a kept tab reached the screen as nothing and
/// tab-indented files lost their indentation.
///
/// The bidi filter matters as much here as it does for filenames: viewing a
/// remote source file is exactly the "trojan source" setting, where an
/// override can make a line of code read as something other than what it
/// says.
pub(crate) fn sanitize_line(s: &str) -> String {
    use unicode_width::UnicodeWidthChar as _;
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut column = 0usize;
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '\t' {
            let pad = TAB_WIDTH - column % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', pad));
            column += pad;
        } else if unsafe_at(&chars, i) {
            out.push(' ');
            column += 1;
        } else {
            out.push(ch);
            column += ch.width().unwrap_or(0);
        }
    }
    out
}

/// Strip unsafe characters from a user-supplied string before printing it
/// directly to the terminal (e.g. CLI warnings, session listings).
///
/// Returns the input unchanged (as `Cow::Borrowed`) when there is nothing to
/// strip, avoiding an allocation on the common case. Otherwise replaces the
/// offending characters with a space — same policy as [`sanitize`], so a name
/// like `host\x00name` becomes `host name` rather than silently collapsing to
/// `hostname`.
///
/// Unlike [`sanitize`], it doesn't cut long strings: the CLI prints names and
/// paths whole, so they can be copied, and the terminal wraps them.
pub(crate) fn sanitize_display(s: &str) -> Cow<'_, str> {
    scrub(s, |_| false).map_or(Cow::Borrowed(s), Cow::Owned)
}

/// [`sanitize_display`] for one log event: the same characters are
/// replaced, except tabs, and the newline that ends the event. A newline
/// inside it — from a remote file name in a `%`-formatted field — is
/// written as a visible `\n`, as a `?`-formatted field would show it:
/// kept, it let a server name a file so as to forge a log line.
pub(crate) fn sanitize_log(s: &str) -> Cow<'_, str> {
    let (body, end) = match s.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (s, ""),
    };
    let scrubbed = scrub(body, |c| c == '\t');
    if scrubbed.is_none() && !body.contains('\n') {
        return Cow::Borrowed(s);
    }
    // `scrub` turned interior newlines into spaces; spell them out instead.
    let mut out = String::with_capacity(s.len() + 8);
    let replaced = scrubbed.as_deref().unwrap_or(body);
    for (orig, new) in body.chars().zip(replaced.chars()) {
        if orig == '\n' {
            out.push_str("\\n");
        } else {
            out.push(new);
        }
    }
    out.push_str(end);
    Cow::Owned(out)
}

/// A log writer that passes everything through [`sanitize_log`].
///
/// The debug log carries server-supplied text — remote paths, file names,
/// error messages — in tracing fields that nothing else sanitizes, so a name
/// holding an escape sequence reached the file intact and ran in the
/// terminal of whoever `cat`s or `tail`s it. tracing-subscriber formats each
/// event whole and writes it in one call, so a call never splits a
/// character, and its one trailing newline is the event's end — which is
/// what lets [`sanitize_log`] tell it from a forged one.
pub(crate) struct SanitizedLog<W>(pub W);

impl<W: io::Write> io::Write for SanitizedLog<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.0.write_all(sanitize_log(&text).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub type Result<T> = std::result::Result<T, BlinkError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_writer_neutralises_escapes_and_keeps_its_layout() {
        use std::io::Write as _;
        let mut out = SanitizedLog(Vec::new());
        out.write_all("remote=/a\x1b[2Jb\u{202E}c\tok\n".as_bytes())
            .unwrap();
        assert_eq!(String::from_utf8(out.0).unwrap(), "remote=/a [2Jb c\tok\n");
    }

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
    fn sanitize_cuts_a_long_string_keeping_both_ends() {
        // Cut at the end, `upload failed: <long path>: permission denied`
        // lost the reason, which is the part that says what went wrong.
        let long = format!("upload failed: /{}/f: permission denied", "x".repeat(1000));
        let out = sanitize(long);
        assert_eq!(out.chars().count(), MAX_SANITIZED_CHARS);
        assert!(out.starts_with("upload failed: /xxx"), "{out}");
        assert!(out.ends_with("/f: permission denied"), "{out}");
        assert_eq!(out.matches('…').count(), 1);
        assert_eq!(
            sanitize(out.clone()),
            out,
            "running it twice changes nothing"
        );
    }

    #[test]
    fn sanitize_exact_limit_not_truncated() {
        let s = "b".repeat(MAX_SANITIZED_CHARS);
        let out = sanitize(s.clone());
        assert_eq!(out, s);
    }

    // Tabs are expanded, not kept: ratatui drops every control grapheme when
    // it renders, `\t` included, so a kept tab reached the screen as nothing
    // and tab-indented files (Makefiles, Go) lost their indentation.

    #[test]
    fn sanitize_line_expands_a_leading_tab_to_the_first_stop() {
        assert_eq!(sanitize_line("\tx"), "        x");
    }

    #[test]
    fn sanitize_line_pads_a_tab_to_the_next_stop() {
        assert_eq!(sanitize_line("ab\tc"), "ab      c");
        assert_eq!(sanitize_line("col1\tcol2"), "col1    col2");
    }

    #[test]
    fn sanitize_line_tab_at_a_stop_takes_a_whole_stop() {
        assert_eq!(sanitize_line("12345678\tx"), "12345678        x");
        assert_eq!(sanitize_line("\t\tx"), format!("{}x", " ".repeat(16)));
    }

    #[test]
    fn sanitize_line_strips_control_but_expands_tab() {
        // The replaced control character is one column, as it renders.
        assert_eq!(sanitize_line("a\x01b\tc"), "a b     c");
    }

    #[test]
    fn an_expanded_line_survives_rendering() {
        use ratatui::style::Style;
        use ratatui::text::Span;
        let line = sanitize_line("\tindented");
        let rendered: String = Span::raw(line)
            .styled_graphemes(Style::default())
            .map(|g| g.symbol)
            .collect();
        assert_eq!(rendered, "        indented");
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
            '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
            '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
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

    /// Other characters that draw as nothing let two names render alike
    /// just as the zero-width ones do.
    #[test]
    fn sanitize_strips_the_other_invisible_characters() {
        for ch in [
            '\u{00AD}',
            '\u{115F}',
            '\u{1160}',
            '\u{180E}',
            '\u{2060}',
            '\u{2064}',
            '\u{206A}',
            '\u{206F}',
            '\u{3164}',
            '\u{FFA0}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{1D173}',
            '\u{E0001}',
            '\u{E0041}',
            '\u{E007F}',
        ] {
            let out = sanitize(format!("a{ch}b"));
            assert_eq!(out, "a b", "U+{:04X} must be neutralised", ch as u32);
        }
    }

    #[test]
    fn sanitize_keeps_what_emoji_need() {
        for s in ["👩\u{200D}💻", "❤\u{FE0F}"] {
            assert_eq!(sanitize(s.to_string()), s);
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

    // -- audit fixes ---------------------------------------------------------

    /// A remote file name holding a newline, logged through a `%` field,
    /// started a line of its own: a server could forge log entries.
    #[test]
    fn a_newline_inside_a_log_event_is_spelled_out() {
        use std::io::Write as _;
        let mut out = SanitizedLog(Vec::new());
        out.write_all(b"WARN remote=x\n2026-11-19T10:00:00Z ERROR fake\n")
            .unwrap();
        let written = String::from_utf8(out.0).unwrap();
        assert_eq!(
            written, "WARN remote=x\\n2026-11-19T10:00:00Z ERROR fake\n",
            "one event, one line"
        );
        assert_eq!(sanitize_log("plain\n"), "plain\n");
        assert!(matches!(sanitize_log("plain\n"), Cow::Borrowed(_)));
    }

    #[test]
    fn sanitize_replaces_more_invisible_characters() {
        for ch in [
            '\u{034F}',
            '\u{17B4}',
            '\u{17B5}',
            '\u{2028}',
            '\u{2029}',
            '\u{2800}',
            '\u{1BCA0}',
            '\u{1BCA3}',
        ] {
            let s = format!("a{ch}b");
            assert_eq!(sanitize(s.clone()), "a b", "U+{:04X}", ch as u32);
            assert_eq!(sanitize_display(&s), "a b", "U+{:04X}", ch as u32);
        }
    }

    /// After a plain letter a variation selector draws as nothing, so
    /// `a` + U+FE0F looked exactly like `a`.
    #[test]
    fn variation_selectors_are_kept_only_where_they_do_something() {
        for kept in [
            "❤\u{FE0F}",         // emoji presentation
            "1\u{FE0F}\u{20E3}", // keycap 1️⃣
            "#\u{FE0F}\u{20E3}", // keycap #️⃣
            "≤\u{FE00}",         // a standardized symbol variant
            "漢\u{E0100}",       // an ideograph variant
        ] {
            assert_eq!(sanitize(kept.to_string()), kept, "{kept:?}");
            assert!(
                matches!(sanitize_display(kept), Cow::Borrowed(_)),
                "unchanged, so not copied"
            );
            assert_eq!(sanitize_display(kept), kept, "{kept:?}");
        }
        for (aliased, shown) in [
            ("a\u{FE0F}", "a "),
            ("\u{FE0F}x", " x"),
            ("1\u{FE0F}", "1 "),
            ("a\u{E0100}", "a "),
            (" \u{FE0F}", "  "),
            ("❤\u{E0100}", "❤ "),
        ] {
            assert_eq!(sanitize(aliased.to_string()), shown, "{aliased:?}");
            assert_eq!(sanitize_line(aliased), shown, "{aliased:?}");
        }
    }

    /// Counted in characters, a wide character before a tab put that
    /// line's later stops a column off.
    #[test]
    fn tab_stops_count_wide_characters_as_two_columns() {
        assert_eq!(sanitize_line("日本\tx"), "日本    x");
        assert_eq!(sanitize_line("ab\tx"), "ab      x");
    }

    #[test]
    fn only_real_config_problems_are_called_config_errors() {
        assert_eq!(
            BlinkError::other("could not read f: x").to_string(),
            "could not read f: x"
        );
        assert_eq!(BlinkError::config("bad").to_string(), "config error: bad");
        assert_eq!(
            BlinkError::InvalidUrl("x".into()).to_string(),
            "invalid URL: x"
        );
    }
}
