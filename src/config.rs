//! Global configuration loaded from `config.ini`.
//!
//! ```ini
//! [general]
//! theme = dracula
//! parallel_downloads = 2
//! confirm_quit = true
//!
//! [terminal]
//! ; auto | kitty | sixel | iterm2 | none
//! image_preview = auto
//! ; false: no mouse capture, the terminal's own selection
//! mouse = true
//! ```
//!
//! rust-ini is built without inline comments: a `;` after a value is part
//! of the value, so comments go on lines of their own.
//!
//! blink changes the file only through [`save_setting`], which edits the
//! one line it's setting and leaves the rest alone.

use std::fs;
use std::path::Path;

use ini::Ini;

use crate::error::{BlinkError, Result};
use crate::paths;

/// Hard ceiling on parallel downloads, per the spec.
pub const MAX_PARALLEL: u8 = 10;

/// Maximum config file size accepted on load (64 KiB).
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub general: General,
    pub terminal: Terminal,
}

#[derive(Debug, Clone)]
pub struct General {
    pub theme: String,
    pub parallel_downloads: u8,
    pub confirm_quit: bool,
}

#[derive(Debug, Clone)]
pub struct Terminal {
    pub image_preview: ImagePreviewMode,
    /// Mouse support. Off leaves mouse capture off, so the terminal's own
    /// text selection works and blink ignores the mouse.
    pub mouse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePreviewMode {
    Auto,
    Kitty,
    Sixel,
    Iterm2,
    None,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General {
                theme: "dracula".to_string(),
                parallel_downloads: 2,
                confirm_quit: true,
            },
            terminal: Terminal {
                image_preview: ImagePreviewMode::Auto,
                mouse: true,
            },
        }
    }
}

/// The settings blink reads, by section. Anything else in the file is kept
/// (saving leaves it alone) but reported, since a mistyped key is
/// otherwise silently ignored.
const KNOWN: &[(&str, &[&str])] = &[
    ("general", &["theme", "parallel_downloads", "confirm_quit"]),
    ("terminal", &["image_preview", "mouse"]),
];

impl Config {
    /// Load `config.ini` from the standard location, with warnings about
    /// what in it was ignored or corrected, for the caller to show.
    ///
    /// If the file does not exist, a commented default is written — first
    /// run, so the user has a documented file to edit. A failure to write
    /// it falls through to in-memory defaults rather than refusing to
    /// launch.
    pub fn load() -> Result<(Self, Vec<String>)> {
        let path = paths::config_file()?;
        match Self::load_with_warnings(&path) {
            Ok(loaded) => Ok(loaded),
            Err(BlinkError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(e) = write_default() {
                    tracing::warn!(?path, "could not write default config: {e}");
                }
                Ok((Self::default(), Vec::new()))
            }
            Err(e) => Err(e),
        }
    }

    /// Parse `path`, dropping the warnings. See [`Self::load_with_warnings`].
    pub fn load_from(path: &Path) -> Result<Self> {
        Self::load_with_warnings(path).map(|(cfg, _)| cfg)
    }

    /// Parse `path`. Any problem with its contents names the file, as
    /// `config.ini: parallel_downloads must be…` — said without it, the
    /// message didn't say where to look. A missing file stays an I/O
    /// `NotFound`, which [`Self::load`] reads as a first run.
    pub fn load_with_warnings(path: &Path) -> Result<(Self, Vec<String>)> {
        let shown = path.display().to_string();
        match Self::parse_file(path) {
            Ok((cfg, warnings)) => Ok((
                cfg,
                warnings
                    .into_iter()
                    .map(|w| format!("{shown}: {w}"))
                    .collect(),
            )),
            Err(BlinkError::Config(msg)) if !msg.starts_with(&shown) => {
                Err(BlinkError::config(format!("{shown}: {msg}")))
            }
            Err(BlinkError::Io(e)) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(BlinkError::other(format!("could not read {shown}: {e}")))
            }
            Err(other) => Err(other),
        }
    }

    fn parse_file(path: &Path) -> Result<(Self, Vec<String>)> {
        let raw = read_capped(path, MAX_CONFIG_BYTES)?;
        // A byte-order mark (Notepad's "UTF-8 with BOM") glued itself to
        // the first header, so `[general]` wasn't found and every setting in
        // it was silently ignored.
        let text = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        let ini = Ini::load_from_str(text)
            .map_err(|e| BlinkError::config(format!("{}: {e}", path.display())))?;
        let mut cfg = Self::default();
        let mut warnings = Vec::new();
        let mut seen: Vec<(String, String)> = Vec::new();

        // Every section in file order, repeats included, so a second
        // `[general]` isn't silently skipped either.
        for (section, props) in ini.iter() {
            let Some(section) = section else {
                for (key, _) in props.iter() {
                    warnings.push(format!("`{key}` is outside any [section]; ignored"));
                }
                continue;
            };
            let Some(&(_, keys)) = KNOWN.iter().find(|(name, _)| *name == section) else {
                warnings.push(
                    match KNOWN.iter().find(|(n, _)| n.eq_ignore_ascii_case(section)) {
                        Some((name, _)) => format!(
                            "[{section}] is ignored: section names are case-sensitive, \
                         did you mean [{name}]?"
                        ),
                        None => format!("unknown section [{section}]; ignored"),
                    },
                );
                continue;
            };
            for (key, value) in props.iter() {
                if !keys.contains(&key) {
                    warnings.push(match keys.iter().find(|k| k.eq_ignore_ascii_case(key)) {
                        Some(k) => format!(
                            "`{key}` in [{section}] is ignored: setting names are \
                             case-sensitive, did you mean `{k}`?"
                        ),
                        None => format!("unknown setting `{key}` in [{section}]; ignored"),
                    });
                    continue;
                }
                let id = (section.to_string(), key.to_string());
                if seen.contains(&id) {
                    warnings.push(format!(
                        "`{key}` in [{section}] is set more than once; the first is used"
                    ));
                    continue;
                }
                seen.push(id);
                cfg.apply(section, key, value, &mut warnings)?;
            }
        }
        Ok((cfg, warnings))
    }

    /// Take one known setting's value.
    fn apply(
        &mut self,
        section: &str,
        key: &str,
        value: &str,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let v = value.trim();
        match (section, key) {
            // Not validated here: a theme that can't be used falls back to
            // dracula with a warning when it's loaded, as a missing one
            // does, rather than stopping blink. `Theme::load` validates.
            ("general", "theme") => self.general.theme = v.to_string(),
            ("general", "parallel_downloads") => {
                // Parsed wider than the field it lands in: as a `u8`, a
                // value above 255 failed to parse and took the whole config
                // with it, while 11..=255 clamped quietly — an accidental
                // cliff at a number nothing else in blink cares about. Any
                // integer now clamps; only a non-number is an error.
                let n: u32 = v.parse().map_err(|_| {
                    BlinkError::config(format!(
                        "parallel_downloads must be an integer between 1 and {MAX_PARALLEL}, \
                         not `{v}`"
                    ))
                })?;
                let clamped = n.clamp(1, u32::from(MAX_PARALLEL)) as u8;
                if u32::from(clamped) != n {
                    warnings.push(format!(
                        "parallel_downloads = {n} is out of range (1 to {MAX_PARALLEL}); \
                         using {clamped}"
                    ));
                }
                self.general.parallel_downloads = clamped;
            }
            ("general", "confirm_quit") => self.general.confirm_quit = parse_bool(key, v)?,
            ("terminal", "image_preview") => {
                self.terminal.image_preview = match v.to_ascii_lowercase().as_str() {
                    "auto" => ImagePreviewMode::Auto,
                    "kitty" => ImagePreviewMode::Kitty,
                    "sixel" => ImagePreviewMode::Sixel,
                    "iterm2" => ImagePreviewMode::Iterm2,
                    "none" | "off" | "false" => ImagePreviewMode::None,
                    _ => {
                        return Err(BlinkError::config(format!(
                            "image_preview must be one of: auto, kitty, sixel, iterm2, none, \
                             not `{v}`"
                        )));
                    }
                };
            }
            ("terminal", "mouse") => self.terminal.mouse = parse_bool(key, v)?,
            _ => unreachable!("only known keys reach apply"),
        }
        Ok(())
    }
}

/// Set one setting in `config.ini`, leaving the rest of the file as it is.
///
/// The file is edited as text: the setting's line has its value replaced,
/// or a line is added to its section (or the section to the file). So
/// comments, blank lines, layout, settings blink doesn't know, and changes
/// made to the file while blink was running all survive — rewriting the
/// whole file from what was loaded lost all of those on every `t`. Nothing
/// has to parse for this to work; a file that can't be read at all is left
/// alone and the save fails.
///
/// A symlinked `config.ini` (dotfiles) is written through to its target:
/// renaming a new file over the link replaced it with a plain file, cut
/// off from the dotfiles.
pub fn save_setting(section: &str, key: &str, value: &str) -> Result<()> {
    if value.contains(['\n', '\r']) {
        return Err(BlinkError::other(format!(
            "`{key}` can't be saved: its value holds a line break"
        )));
    }
    let path = write_target(&paths::config_file()?)?;
    let text = match read_capped(&path, MAX_CONFIG_BYTES) {
        Ok(text) => text,
        Err(BlinkError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => default_text(),
        Err(e) => {
            return Err(BlinkError::other(format!(
                "not saving `{key}`, so as not to overwrite {}: {e}",
                path.display()
            )));
        }
    };
    let edited = set_in_text(&text, section, key, value);
    paths::atomic_write(&path, |f| {
        use std::io::Write as _;
        f.write_all(edited.as_bytes())
    })
}

/// Write the commented default `config.ini`.
fn write_default() -> Result<()> {
    let path = write_target(&paths::config_file()?)?;
    let text = default_text();
    paths::atomic_write(&path, |f| {
        use std::io::Write as _;
        f.write_all(text.as_bytes())
    })
}

/// The first-run `config.ini`: the defaults, each with what it does.
fn default_text() -> String {
    let d = Config::default();
    format!(
        "; blink settings. Lines starting with ; or # are comments; a comment\n\
         ; can't follow a value on the same line.\n\
         \n\
         [general]\n\
         ; dracula, aura, nord, solarized-dark, solarized-osaka, tokyo-night,\n\
         ; cyberpunk-neon, or the name of a theme file in the themes folder.\n\
         ; `t` in blink cycles through them and saves the choice here.\n\
         theme = {theme}\n\
         ; Transfers to run at once, 1 to {MAX_PARALLEL}. A session can set its own.\n\
         parallel_downloads = {parallel}\n\
         ; Ask before quitting.\n\
         confirm_quit = {confirm}\n\
         \n\
         [terminal]\n\
         ; How to show images: auto, kitty, sixel, iterm2 or none.\n\
         image_preview = auto\n\
         ; false leaves the mouse to the terminal, for its own text selection.\n\
         mouse = {mouse}\n",
        theme = d.general.theme,
        parallel = d.general.parallel_downloads,
        confirm = d.general.confirm_quit,
        mouse = d.terminal.mouse,
    )
}

/// The file a write to `path` should go to: `path`, or if it's a symlink
/// (a chain of them, even a dangling one), what it points at.
fn write_target(path: &Path) -> Result<std::path::PathBuf> {
    let mut p = path.to_path_buf();
    for _ in 0..16 {
        match fs::symlink_metadata(&p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&p)?;
                p = match p.parent() {
                    Some(dir) if link.is_relative() => dir.join(link),
                    _ => link,
                };
            }
            _ => return Ok(p),
        }
    }
    Err(BlinkError::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// `text` with `key` in `[section]` set to `value`, everything else kept:
/// the first such line (the one the loader reads) gets the new value after
/// its own `key =` spelling; without one, a `key = value` line goes after
/// the section's last setting; without the section, it's appended. A
/// byte-order mark and the file's line endings are kept.
fn set_in_text(text: &str, section: &str, key: &str, value: &str) -> String {
    let (bom, body) = match text.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", text),
    };
    let nl = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = body.split_inclusive('\n').map(str::to_string).collect();

    let mut current: Option<String> = None;
    let mut insert_at: Option<usize> = None;
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim_end_matches(['\n', '\r']);
        let t = line.trim();
        if t.starts_with('[')
            && let Some(end) = t.find(']')
        {
            current = Some(t[1..end].trim().to_string());
            if current.as_deref() == Some(section) {
                insert_at = Some(i + 1);
            }
            continue;
        }
        if current.as_deref() != Some(section)
            || t.is_empty()
            || t.starts_with(';')
            || t.starts_with('#')
        {
            continue;
        }
        insert_at = Some(i + 1);
        if let Some(sep) = line.find(['=', ':'])
            && line[..sep].trim() == key
        {
            let after = &line[sep + 1..];
            let gap = &after[..after.len() - after.trim_start().len()];
            let ending = &raw[line.len()..];
            lines[i] = format!("{}{gap}{value}{ending}", &line[..sep + 1]);
            return format!("{bom}{}", lines.concat());
        }
    }

    let new_line = format!("{key} = {value}{nl}");
    match insert_at {
        Some(at) => {
            if at > 0 && !lines[at - 1].ends_with('\n') {
                lines[at - 1].push_str(nl);
            }
            lines.insert(at, new_line);
        }
        None => {
            if let Some(last) = lines.last_mut()
                && !last.ends_with('\n')
            {
                last.push_str(nl);
            }
            if !lines.is_empty() {
                lines.push(nl.to_string());
            }
            lines.push(format!("[{section}]{nl}"));
            lines.push(new_line);
        }
    }
    format!("{bom}{}", lines.concat())
}

/// Read `path`, refusing anything larger than `max` bytes.
///
/// Uses the take() pattern (open then read) rather than metadata() +
/// read_to_string() so the file cannot grow between the two syscalls. I/O
/// errors pass through unchanged, so callers can still tell `NotFound`
/// (first run) from a real failure.
fn read_capped(path: &Path, max: u64) -> Result<String> {
    use std::io::Read as _;
    let file = fs::File::open(path)?;
    let mut raw = String::new();
    file.take(max + 1).read_to_string(&mut raw)?;
    if raw.len() as u64 > max {
        return Err(BlinkError::config(format!(
            "config file is too large (limit is {max} bytes)"
        )));
    }
    Ok(raw)
}

/// Validate a theme name from the config file.
///
/// Theme names are used to build file paths (`themes_dir/<name>.ini`), so we
/// must reject values that could traverse outside the themes directory.
/// Allowed: alphanumerics, `-`, `_`, `.` (single dots, not `..`).
pub(crate) fn validate_theme_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(BlinkError::config("theme name must not be empty"));
    }
    // Reject path separators and null bytes outright. `:` joins them: on
    // Windows a component carrying a drive prefix but no root replaces the
    // whole buffer, so `themes_dir().join("C:evil")` resolves outside the
    // themes directory. `safe_local_name_for` documents and blocks the same
    // hazard for server-supplied download names, where it is Windows-only
    // because a colon is a legal Unix filename character. Here it is refused
    // everywhere, as `\` already is: a theme name is an identifier the user
    // types into their own config, not a filename.
    if name.contains(['/', '\\', '\0', ':']) {
        return Err(BlinkError::config(
            "theme name must not contain path separators",
        ));
    }
    // Control characters can't be typed into a config sensibly, and a
    // theme file named with one put raw escapes on the terminal when
    // `blink themes` listed it.
    if name.chars().any(char::is_control) {
        return Err(BlinkError::config(
            "theme name must not contain control characters",
        ));
    }
    // Reject `..` anywhere in the name to block traversal via dots.
    // Path separators are already rejected above, so split-based checks
    // are unnecessary; `contains` is sufficient.
    if name.contains("..") {
        return Err(BlinkError::config("theme name must not contain '..'"));
    }
    Ok(())
}

/// A yes/no setting. The error names the setting and what was written:
/// "boolean value must be…" alone didn't say which of them was wrong.
fn parse_bool(key: &str, s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(BlinkError::config(format!(
            "{key} must be true or false (or yes/no, on/off, 1/0), not `{}`",
            s.trim()
        ))),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // -- mouse ------------------------------------------------------------------

    /// Load `body` as a config.ini in a private test home. `_tag` is kept
    /// so call sites read as before; the home is what keeps them apart.
    fn load_str(_tag: &str, body: &str) -> Result<Config> {
        load_warned(body).map(|(cfg, _)| cfg)
    }

    fn load_warned(body: &str) -> Result<(Config, Vec<String>)> {
        let home = crate::paths::test_home();
        let path = home.path().join("config.ini");
        std::fs::write(&path, body).unwrap();
        Config::load_with_warnings(&path)
    }

    #[test]
    fn the_first_load_writes_a_default_config_to_edit() {
        let _home = crate::paths::test_home();
        let path = crate::paths::config_file().unwrap();
        assert!(!path.exists());

        let (cfg, _) = Config::load().unwrap();

        assert_eq!(cfg.general.theme, Config::default().general.theme);
        assert!(path.exists(), "a default config.ini is written");
        let (again, _) = Config::load().unwrap();
        assert_eq!(again.general.theme, cfg.general.theme);
    }

    #[test]
    fn load_reads_the_users_config() {
        let _home = crate::paths::test_home();
        std::fs::write(
            crate::paths::config_file().unwrap(),
            "[general]\ntheme = nord\nparallel_downloads = 3\n",
        )
        .unwrap();
        let (cfg, _) = Config::load().unwrap();
        assert_eq!(cfg.general.theme, "nord");
        assert_eq!(cfg.general.parallel_downloads, 3);
    }

    #[test]
    fn load_refuses_a_config_it_cannot_read() {
        let _home = crate::paths::test_home();
        std::fs::write(
            crate::paths::config_file().unwrap(),
            "[general]\nparallel_downloads = lots\n",
        )
        .unwrap();
        let err = Config::load().unwrap_err();
        assert!(err.to_string().contains("parallel_downloads"), "{err}");
    }

    /// The first ```ini block after `heading` in the README.
    pub(crate) fn readme_ini_block(heading: &str) -> String {
        let readme = include_str!("../README.md");
        let after = &readme[readme.find(heading).expect("heading in README")..];
        let start = after.find("```ini\n").expect("an ini block") + "```ini\n".len();
        let len = after[start..].find("```").expect("the block ends");
        after[start..start + len].to_string()
    }

    /// Someone copying the README's example must get a blink that starts.
    /// rust-ini is built without inline comments, so `key = value ; note`
    /// reads the note as part of the value: `mouse = true ; …` was not a
    /// boolean, and `Config::load` refused to start.
    #[test]
    fn the_readme_config_example_loads() {
        let cfg = load_str("readme", &readme_ini_block("### `config.ini` — global"))
            .expect("the documented example must load");
        assert_eq!(cfg.general.theme, "dracula");
        assert!(cfg.terminal.mouse);
    }

    #[test]
    fn mouse_is_on_unless_turned_off() {
        assert!(Config::default().terminal.mouse);
        assert!(
            load_str("mouse-absent", "[terminal]\nimage_preview = auto\n")
                .unwrap()
                .terminal
                .mouse
        );
        assert!(
            !load_str("mouse-off", "[terminal]\nmouse = false\n")
                .unwrap()
                .terminal
                .mouse
        );
        assert!(
            load_str("mouse-on", "[terminal]\nmouse = yes\n")
                .unwrap()
                .terminal
                .mouse
        );
    }

    #[test]
    fn a_mouse_value_that_is_not_a_boolean_is_refused() {
        assert!(load_str("mouse-bad", "[terminal]\nmouse = sometimes\n").is_err());
    }

    #[test]
    fn saving_one_setting_keeps_the_others() {
        let _home = crate::paths::test_home();
        let path = paths::config_file().unwrap();
        fs::write(&path, "[terminal]\nmouse = false\n").unwrap();
        save_setting("general", "theme", "nord").unwrap();
        let back = Config::load_from(&path).unwrap();
        assert!(!back.terminal.mouse);
        assert_eq!(back.general.theme, "nord");
    }

    // -- parallel_downloads clamping ---------------------------------------
    //
    // The cap exists because each parallel download holds its own
    // connection. An out-of-range value in a hand-edited or stale file is
    // corrected rather than rejected, so a bad number never stops blink
    // from starting.

    fn parallel_from(tag: &str, value: &str) -> Result<u8> {
        load_str(tag, &format!("[general]\nparallel_downloads = {value}\n"))
            .map(|c| c.general.parallel_downloads)
    }

    #[test]
    fn a_value_above_the_maximum_is_clamped_to_it() {
        assert_eq!(
            parallel_from("high", "50").expect("must load"),
            MAX_PARALLEL
        );
    }

    #[test]
    fn a_value_too_large_for_a_byte_is_clamped_rather_than_rejected() {
        // 300 used to fail the `u8` parse and take the whole config down
        // with it, even though the README promises an out-of-range value is
        // clamped so a stale file cannot stop blink from starting.
        assert_eq!(
            parallel_from("huge", "300").expect("must load"),
            MAX_PARALLEL
        );
    }

    #[test]
    fn zero_is_clamped_up_to_one() {
        assert_eq!(parallel_from("zero", "0").expect("must load"), 1);
    }

    #[test]
    fn a_value_that_is_not_a_number_is_still_rejected() {
        // Out of range has an obvious correction; a typo does not.
        let err = parallel_from("word", "banana").expect_err("not a number");
        assert!(err.to_string().contains("parallel_downloads"), "{err}");
    }

    // -- bounded reads -----------------------------------------------------
    //
    // `load_from` capped the file it parsed; `save` read the same file back
    // with an unbounded `read_to_string` to preserve unknown keys. Same file,
    // two different ideas of how big it may be.

    /// A file of `bytes` bytes in `home`, which keeps it private to the
    /// test and removes it afterwards.
    fn tmp(home: &crate::paths::TestHome, bytes: usize) -> std::path::PathBuf {
        let path = home.path().join("config.ini");
        std::fs::write(&path, "x".repeat(bytes)).unwrap();
        path
    }

    #[test]
    fn a_file_within_the_cap_reads_back() {
        let home = crate::paths::test_home();
        let path = tmp(&home, 16);
        let raw = read_capped(&path, MAX_CONFIG_BYTES).expect("must read");
        assert_eq!(raw.len(), 16);
    }

    #[test]
    fn a_file_at_the_cap_reads_back() {
        let home = crate::paths::test_home();
        let path = tmp(&home, MAX_CONFIG_BYTES as usize);
        assert!(read_capped(&path, MAX_CONFIG_BYTES).is_ok());
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let home = crate::paths::test_home();
        let path = tmp(&home, MAX_CONFIG_BYTES as usize + 1);
        let err = read_capped(&path, MAX_CONFIG_BYTES).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[test]
    fn a_missing_file_reports_not_found() {
        let err = read_capped(std::path::Path::new("/nonexistent/blink.ini"), 64).unwrap_err();
        assert!(
            matches!(err, BlinkError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound),
            "callers distinguish first-run from a real failure: {err:?}",
        );
    }

    // parse_bool
    #[test]
    fn parse_bool_truthy_values() {
        for s in &["1", "true", "yes", "on", "TRUE", "Yes", " on "] {
            assert!(parse_bool("k", s).unwrap(), "expected true for {s:?}");
        }
    }

    #[test]
    fn parse_bool_falsy_values() {
        for s in &["0", "false", "no", "off", "FALSE", "No", " off "] {
            assert!(!parse_bool("k", s).unwrap(), "expected false for {s:?}");
        }
    }

    #[test]
    fn parse_bool_invalid_errors() {
        assert!(parse_bool("k", "maybe").is_err());
        assert!(parse_bool("k", "").is_err());
        assert!(parse_bool("k", "2").is_err());
    }

    /// `config error: parallel_downloads must be…` didn't say which file.
    #[test]
    fn a_problem_in_config_ini_names_the_file() {
        let _home = crate::paths::test_home();
        let path = crate::paths::config_file().unwrap();
        for (body, says) in [
            ("[general]\nparallel_downloads = x\n", "parallel_downloads"),
            ("[terminal]\nmouse = maybe\n", "mouse"),
        ] {
            fs::write(&path, body).unwrap();
            let e = Config::load_from(&path).unwrap_err().to_string();
            assert!(
                e.starts_with(&format!("config error: {}: ", path.display())),
                "{e}"
            );
            assert!(e.contains(says), "{e}");
        }
        // A missing file is still the first run, not an error.
        fs::remove_file(&path).unwrap();
        assert!(Config::load().is_ok());
    }

    // validate_theme_name
    #[test]
    fn theme_name_valid() {
        assert!(validate_theme_name("dracula").is_ok());
        assert!(validate_theme_name("tokyo-night").is_ok());
        assert!(validate_theme_name("my_theme.1").is_ok());
    }

    #[test]
    fn theme_name_empty_errors() {
        assert!(validate_theme_name("").is_err());
    }

    #[test]
    fn theme_name_path_separator_errors() {
        assert!(validate_theme_name("../../etc/passwd").is_err());
        assert!(validate_theme_name("a/b").is_err());
    }

    #[test]
    fn theme_name_dotdot_errors() {
        assert!(validate_theme_name("..").is_err());
        assert!(validate_theme_name("a..b").is_err());
    }

    /// A component carrying a drive prefix but no root replaces the whole
    /// buffer on Windows, so `themes_dir().join("C:evil")` resolves outside
    /// the themes directory entirely — the same hazard `safe_local_name_for`
    /// documents and blocks for server-supplied download names. Rejected on
    /// every platform, as `\\` already is: a theme name is an identifier the
    /// user types into their own config, not a filename, and none of them
    /// legitimately carries a colon.
    #[test]
    fn theme_name_colon_errors() {
        assert!(validate_theme_name("C:evil").is_err());
        assert!(validate_theme_name("a:b").is_err());
    }

    /// A theme file named with an escape sequence put it raw on the
    /// terminal when `blink themes` listed it.
    #[test]
    fn theme_name_control_characters_error() {
        assert!(validate_theme_name("a\x1b[31mred").is_err());
        assert!(validate_theme_name("tab\there").is_err());
        assert!(validate_theme_name("del\u{7f}").is_err());
    }

    #[test]
    fn theme_name_null_byte_errors() {
        assert!(validate_theme_name("evil\0theme").is_err());
    }

    // -- saving one setting -------------------------------------------------

    /// Rewritten from what was loaded, every save dropped comments, blank
    /// lines and layout (rust-ini doesn't keep comments).
    #[test]
    fn saving_keeps_comments_layout_and_unknown_settings() {
        let text = "; my note\n[general]\n# why nord\ntheme   =   nord\nmy_flag=1\n\n[terminal]\nmouse = false\n";
        assert_eq!(
            set_in_text(text, "general", "theme", "aura"),
            "; my note\n[general]\n# why nord\ntheme   =   aura\nmy_flag=1\n\n[terminal]\nmouse = false\n"
        );
    }

    #[test]
    fn saving_adds_a_missing_setting_or_section() {
        assert_eq!(
            set_in_text(
                "[general]\nconfirm_quit = no\n\n[terminal]\n",
                "general",
                "theme",
                "nord"
            ),
            "[general]\nconfirm_quit = no\ntheme = nord\n\n[terminal]\n"
        );
        assert_eq!(
            set_in_text("[terminal]\nmouse = no", "general", "theme", "nord"),
            "[terminal]\nmouse = no\n\n[general]\ntheme = nord\n"
        );
        assert_eq!(
            set_in_text("", "general", "theme", "nord"),
            "[general]\ntheme = nord\n"
        );
        // Into the first of two [general]s it isn't in: after the last.
        assert_eq!(
            set_in_text(
                "[general]\na = 1\n[x]\n[general]\nb = 2\n",
                "general",
                "theme",
                "t"
            ),
            "[general]\na = 1\n[x]\n[general]\nb = 2\ntheme = t\n"
        );
    }

    /// The first occurrence is the one the loader reads, so it's the one
    /// changed; a key in another section is left alone.
    #[test]
    fn saving_changes_the_line_the_loader_reads() {
        assert_eq!(
            set_in_text(
                "[x]\ntheme = a\n[general]\ntheme = b\ntheme = c\n",
                "general",
                "theme",
                "z"
            ),
            "[x]\ntheme = a\n[general]\ntheme = z\ntheme = c\n"
        );
    }

    #[test]
    fn saving_keeps_crlf_and_a_byte_order_mark() {
        assert_eq!(
            set_in_text(
                "\u{FEFF}[general]\r\ntheme = a\r\n",
                "general",
                "theme",
                "b"
            ),
            "\u{FEFF}[general]\r\ntheme = b\r\n"
        );
        assert_eq!(
            set_in_text("[general]\r\nx = 1\r\n", "general", "theme", "b"),
            "[general]\r\nx = 1\r\ntheme = b\r\n"
        );
    }

    /// Writing everything from memory undid edits made to the file while
    /// blink was running.
    #[test]
    fn saving_keeps_edits_made_since_loading() {
        let _home = crate::paths::test_home();
        let path = paths::config_file().unwrap();
        fs::write(&path, "[general]\ntheme = nord\n[terminal]\nmouse = true\n").unwrap();
        let (_loaded, _) = Config::load().unwrap();
        fs::write(
            &path,
            "[general]\ntheme = nord\n[terminal]\nmouse = false\n",
        )
        .unwrap();

        save_setting("general", "theme", "aura").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[general]\ntheme = aura\n[terminal]\nmouse = false\n"
        );
    }

    /// A file that doesn't parse is edited on its one line, not replaced.
    #[test]
    fn saving_into_a_file_that_doesnt_parse_keeps_it() {
        let _home = crate::paths::test_home();
        let path = paths::config_file().unwrap();
        fs::write(&path, "[general\ntheme = nord\nmy_stuff = keep\n").unwrap();
        save_setting("general", "theme", "aura").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("[general\ntheme = nord\nmy_stuff = keep\n"),
            "{text}"
        );
    }

    /// One that can't be read at all is left alone, and the save fails.
    #[test]
    fn saving_refuses_to_overwrite_a_file_it_cant_read() {
        let _home = crate::paths::test_home();
        let path = paths::config_file().unwrap();
        let big = format!("[general]\n; {}\n", "x".repeat(MAX_CONFIG_BYTES as usize));
        fs::write(&path, &big).unwrap();
        let e = save_setting("general", "theme", "aura")
            .unwrap_err()
            .to_string();
        assert!(e.contains("not saving `theme`"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), big);
    }

    /// Renaming over a dotfiles symlink replaced it with a plain file.
    #[cfg(unix)]
    #[test]
    fn saving_writes_through_a_symlink() {
        let home = crate::paths::test_home();
        let path = paths::config_file().unwrap();
        let real = home.path().join("dotfiles-config.ini");
        fs::write(&real, "[general]\ntheme = nord\n").unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        save_setting("general", "theme", "aura").unwrap();

        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "[general]\ntheme = aura\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_first_run_default_goes_through_a_dangling_symlink_too() {
        let home = crate::paths::test_home();
        let path = paths::config_file().unwrap();
        let real = home.path().join("not-yet.ini");
        std::os::unix::fs::symlink("not-yet.ini", &path).unwrap();

        Config::load().unwrap();

        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            real.exists(),
            "the default was written where the link points"
        );
    }

    #[test]
    fn a_value_with_a_line_break_isnt_saved() {
        let _home = crate::paths::test_home();
        assert!(save_setting("general", "theme", "a\nmouse = false").is_err());
    }

    /// The first run's "documented file to edit" had no documentation.
    #[test]
    fn the_first_run_file_is_commented_and_loads_as_the_defaults() {
        let text = default_text();
        assert!(
            text.lines().filter(|l| l.starts_with(';')).count() >= 6,
            "{text}"
        );
        let (cfg, warnings) = load_warned(&text).unwrap();
        let d = Config::default();
        assert_eq!(cfg.general.theme, d.general.theme);
        assert_eq!(cfg.general.parallel_downloads, d.general.parallel_downloads);
        assert_eq!(cfg.general.confirm_quit, d.general.confirm_quit);
        assert_eq!(cfg.terminal.image_preview, d.terminal.image_preview);
        assert_eq!(cfg.terminal.mouse, d.terminal.mouse);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    // -- loading ------------------------------------------------------------

    /// Notepad's byte-order mark hid `[general]`, and every setting in it.
    #[test]
    fn a_byte_order_mark_is_ignored() {
        let cfg = load_str(
            "bom",
            "\u{FEFF}[general]\ntheme = nord\nconfirm_quit = false\n",
        )
        .unwrap();
        assert_eq!(cfg.general.theme, "nord");
        assert!(!cfg.general.confirm_quit);
    }

    /// A mistyped setting was ignored without a word.
    #[test]
    fn what_is_ignored_or_corrected_is_warned_about() {
        let (cfg, warnings) = load_warned(
            "stray = 1\n\
             [general]\n\
             Theme = nord\n\
             confim_quit = false\n\
             parallel_downloads = 20\n\
             theme = aura\n\
             theme = nord\n\
             [General]\n\
             [extras]\n\
             x = 1\n\
             [general]\n\
             confirm_quit = no\n",
        )
        .unwrap();
        let has = |s: &str| warnings.iter().any(|w| w.contains(s));
        assert!(has("`stray` is outside any [section]"), "{warnings:?}");
        assert!(has("`Theme` in [general] is ignored"), "{warnings:?}");
        assert!(has("did you mean `theme`?"), "{warnings:?}");
        assert!(
            has("unknown setting `confim_quit` in [general]"),
            "{warnings:?}"
        );
        assert!(
            has("parallel_downloads = 20 is out of range"),
            "{warnings:?}"
        );
        assert!(
            has("`theme` in [general] is set more than once"),
            "{warnings:?}"
        );
        assert!(has("did you mean [general]?"), "{warnings:?}");
        assert!(has("unknown section [extras]"), "{warnings:?}");
        assert!(
            warnings.iter().all(|w| w.contains("config.ini: ")),
            "{warnings:?}"
        );
        assert_eq!(cfg.general.theme, "aura", "the first is used");
        assert_eq!(cfg.general.parallel_downloads, MAX_PARALLEL);
        assert!(!cfg.general.confirm_quit, "a second [general] is read too");
    }

    #[test]
    fn a_clean_file_has_no_warnings() {
        let (_, warnings) =
            load_warned("[general]\ntheme = nord\n[terminal]\nmouse = no\n").unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// "boolean value must be one of…" didn't say which setting, or what
    /// was written.
    #[test]
    fn errors_name_the_setting_and_the_value() {
        for (body, says) in [
            (
                "[terminal]\nmouse = sometimes\n",
                "mouse must be true or false",
            ),
            (
                "[general]\nconfirm_quit = perhaps\n",
                "confirm_quit must be",
            ),
            ("[terminal]\nimage_preview = vga\n", "not `vga`"),
            ("[general]\nparallel_downloads = lots\n", "not `lots`"),
        ] {
            let e = load_str("err", body).unwrap_err().to_string();
            assert!(e.contains(says), "{e}");
        }
        let e = load_str("err", "[terminal]\nmouse = sometimes\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("not `sometimes`"), "{e}");
    }

    /// `theme = ../x` stopped blink from starting; `theme = missing` fell
    /// back. Both are a theme that can't be used, and now both fall back.
    #[test]
    fn a_theme_name_that_cant_be_used_falls_back_instead_of_stopping_blink() {
        let cfg = load_str("theme", "[general]\ntheme = ../x\n").expect("loads");
        let (theme, warning) = crate::theme::Theme::load_or_fallback(&cfg.general.theme);
        assert_eq!(theme.key, "dracula");
        assert!(warning.unwrap().contains(".."));
    }
}
