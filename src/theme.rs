//! Themes: a small palette of named colors used everywhere the TUI renders.
//!
//! Seven built-in themes ship with the binary. Users can add more by dropping
//! `<name>.ini` into the themes dir (see [`paths::themes_dir`]):
//!
//! ```ini
//! [theme]
//! name = my-theme
//!
//! [colors]
//! bg              = #1a1b26
//! fg              = #c0caf5
//! dim             = #565f89
//! cursor_bg       = #282a36
//! border_active   = #bb9af7
//! border_inactive = #292e42
//! accent          = #f7768e
//! directory       = #7dcfff
//! image           = #f7768e
//! selected        = #e0af68
//! success         = #9ece6a
//! warning         = #ff9e64
//! error           = #db4b4b
//! ```
//!
//! Any colour left out is taken from `dracula`, so a theme written before a
//! colour existed keeps loading.
//!
//! A theme is found by its *key*: the name with surrounding space trimmed,
//! lowercased, and inner spaces turned into hyphens. That one rule applies
//! to built-ins and user files alike, so `Tokyo Night`, `tokyo-night` and
//! `TOKYO-NIGHT` are the same theme, and `My Theme.ini` is `my-theme`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ini::Ini;
use ratatui::style::Color;

use crate::config;
use crate::error::{self, BlinkError, Result};
use crate::paths;

/// Maximum theme file size accepted on load (64 KiB).
const MAX_THEME_BYTES: u64 = 64 * 1024;

/// The theme used when the configured one won't load, and the source of any
/// colour a user theme leaves out.
pub const FALLBACK: &str = "dracula";

#[derive(Debug, Clone)]
pub struct Theme {
    /// What loads this theme again: the built-in name or the normalised
    /// file stem. This is what config.ini stores and what `t` cycles by.
    pub key: String,
    /// Display label: `[theme] name` from the file, else the stem.
    pub name: String,

    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub cursor_bg: Color,

    pub border_active: Color,
    pub border_inactive: Color,

    pub accent: Color,
    pub directory: Color,
    pub image: Color,
    pub selected: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
}

/// One theme `blink themes` and the `t` cycle can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeEntry {
    pub key: String,
    /// Loaded from a file in the themes dir (which shadows a built-in of
    /// the same key).
    pub user: bool,
}

/// Every theme on offer, plus the files in the themes dir whose names can
/// never load.
#[derive(Debug, Default)]
pub struct ThemeListing {
    /// Sorted by key, one entry per key.
    pub entries: Vec<ThemeEntry>,
    /// File names (with extension) refused by `validate_theme_name`.
    pub invalid: Vec<String>,
}

/// Normalise a theme name to its key.
pub fn key_of(name: &str) -> String {
    name.trim().to_lowercase().replace(' ', "-")
}

impl Theme {
    /// Resolve a theme by name. User themes (in the themes dir) shadow
    /// built-ins. The result is downgraded to the 256-colour palette when
    /// the terminal doesn't look able to show 24-bit colour.
    pub fn load(name: &str) -> Result<Self> {
        let theme = Self::load_rgb(name)?;
        Ok(if truecolor() { theme } else { theme.into_256() })
    }

    fn load_rgb(name: &str) -> Result<Self> {
        // Validate before constructing any path — load() may be called from
        // contexts that haven't gone through config/session validation.
        config::validate_theme_name(name)?;
        let key = key_of(name);
        config::validate_theme_name(&key)?;

        if let Some(path) = user_file(&key) {
            // NotFound here means the file went between the directory scan
            // and the open; fall through to the built-ins as if never there.
            match Self::load_from(&path) {
                Ok(mut theme) => {
                    theme.key = key;
                    return Ok(theme);
                }
                Err(BlinkError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }

        builtin(&key).ok_or_else(|| BlinkError::theme_not_found(name))
    }

    /// [`load`](Self::load), falling back to [`FALLBACK`] and saying why.
    /// Used at startup, where the warning has to wait for the log.
    pub fn load_or_fallback(name: &str) -> (Self, Option<String>) {
        match Self::load(name) {
            Ok(theme) => (theme, None),
            Err(e) => {
                let fallback = Self::load(FALLBACK).expect("the fallback is a built-in");
                let shown = error::sanitize(name.to_string());
                let why = match e {
                    BlinkError::ThemeNotFound(_) => format!("theme `{shown}` not found"),
                    e => format!("theme `{shown}` failed to load: {e}"),
                };
                (fallback, Some(format!("{why}; using {FALLBACK}")))
            }
        }
    }

    /// Parse one theme file. Its key is the file's stem as found on disk;
    /// [`load`](Self::load) replaces it with the normalised key.
    pub fn load_from(path: &Path) -> Result<Self> {
        // Enforce a size limit before parsing so a large or malicious theme
        // file cannot exhaust memory.
        let file = fs::File::open(path)?;
        let mut raw = String::new();
        file.take(MAX_THEME_BYTES + 1).read_to_string(&mut raw)?;
        if raw.len() as u64 > MAX_THEME_BYTES {
            return Err(BlinkError::config(format!(
                "{}: theme file is too large (limit is {MAX_THEME_BYTES} bytes)",
                path.display()
            )));
        }

        let ini = Ini::load_from_str(&raw)
            .map_err(|e| BlinkError::config(format!("{}: {e}", path.display())))?;
        let s = ini.section(Some("colors")).ok_or_else(|| {
            BlinkError::config(format!("{}: missing [colors] section", path.display()))
        })?;

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(key_of)
            .unwrap_or_else(|| "custom".to_string());
        // Sanitize the display name so ANSI sequences from a crafted
        // [theme] name field don't reach the TUI status bar.
        let name = ini
            .section(Some("theme"))
            .and_then(|t| t.get("name"))
            .map(|s| error::sanitize(s.to_string()))
            .unwrap_or_else(|| error::sanitize(stem.clone()));

        let base = dracula();
        let g = |key: &str, default: Color| -> Result<Color> {
            let Some(v) = s.get(key) else {
                return Ok(default);
            };
            parse_color(v).ok_or_else(|| {
                BlinkError::config(format!("{}: bad color for `{key}`", path.display()))
            })
        };

        Ok(Self {
            key: stem,
            name,
            bg: g("bg", base.bg)?,
            fg: g("fg", base.fg)?,
            dim: g("dim", base.dim)?,
            cursor_bg: g("cursor_bg", base.cursor_bg)?,
            border_active: g("border_active", base.border_active)?,
            border_inactive: g("border_inactive", base.border_inactive)?,
            accent: g("accent", base.accent)?,
            directory: g("directory", base.directory)?,
            image: g("image", base.image)?,
            selected: g("selected", base.selected)?,
            success: g("success", base.success)?,
            warning: g("warning", base.warning)?,
            error: g("error", base.error)?,
        })
    }

    pub fn list_builtin_names() -> Vec<&'static str> {
        BUILTINS.iter().map(|(k, _)| *k).collect()
    }

    /// Every theme key the user could pick, deduplicated and sorted: the
    /// built-ins plus each loadable-by-name file in the themes dir. Used
    /// for the in-app cycle hotkey.
    ///
    /// Files aren't parsed here, so a listed theme can still fail to load;
    /// `cycle_theme` skips past those.
    pub fn list_all_names() -> Vec<String> {
        Self::list_detailed()
            .entries
            .into_iter()
            .map(|e| e.key)
            .collect()
    }

    /// [`list_all_names`](Self::list_all_names) with where each one comes
    /// from, and the files that were left out because their names can't
    /// load.
    pub fn list_detailed() -> ThemeListing {
        use std::collections::BTreeMap;
        let mut map: BTreeMap<String, bool> = BUILTINS
            .iter()
            .map(|(k, _)| (k.to_string(), false))
            .collect();
        let mut invalid = Vec::new();
        for (stem, path) in user_files() {
            let key = key_of(&stem);
            if config::validate_theme_name(&stem).is_err()
                || config::validate_theme_name(&key).is_err()
            {
                if let Some(f) = path.file_name().and_then(|f| f.to_str()) {
                    invalid.push(error::sanitize(f.to_string()));
                }
                continue;
            }
            map.insert(key, true);
        }
        invalid.sort();
        ThemeListing {
            entries: map
                .into_iter()
                .map(|(key, user)| ThemeEntry { key, user })
                .collect(),
            invalid,
        }
    }

    /// The same theme with every colour mapped to the nearest entry of the
    /// xterm 256-colour palette.
    pub fn into_256(self) -> Self {
        let m = |c: Color| match c {
            Color::Rgb(r, g, b) => Color::Indexed(rgb_to_256(r, g, b)),
            other => other,
        };
        Self {
            key: self.key,
            name: self.name,
            bg: m(self.bg),
            fg: m(self.fg),
            dim: m(self.dim),
            cursor_bg: m(self.cursor_bg),
            border_active: m(self.border_active),
            border_inactive: m(self.border_inactive),
            accent: m(self.accent),
            directory: m(self.directory),
            image: m(self.image),
            selected: m(self.selected),
            success: m(self.success),
            warning: m(self.warning),
            error: m(self.error),
        }
    }
}

/// Every `.ini` in the themes dir, as (stem, path).
///
/// Only reads: a themes folder that doesn't exist yet has no files, and
/// isn't created.
fn user_files() -> Vec<(String, PathBuf)> {
    let Ok(dir) = paths::lookup("themes") else {
        return Vec::new();
    };
    let Ok(read) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("ini"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            Some((stem, p))
        })
        .collect();
    files.sort();
    files
}

/// The file in the themes dir whose key is `key`. A stem already equal to
/// the key wins; otherwise the first, in sorted order, that normalises to
/// it — so the choice doesn't depend on directory order.
fn user_file(key: &str) -> Option<PathBuf> {
    let exact = paths::lookup("themes").ok()?.join(format!("{key}.ini"));
    let files = user_files();
    if files.iter().any(|(_, p)| *p == exact) {
        return Some(exact);
    }
    files
        .into_iter()
        .find(|(stem, _)| key_of(stem) == key)
        .map(|(_, p)| p)
}

/// Parse a CSS-style hex color string (`#rrggbb` or `rrggbb`), allowing
/// surrounding whitespace and at most one `#`.
fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    // Checking the digits up front also rules out non-ASCII, so the byte
    // slices below always fall on char boundaries, and the `+` that
    // `from_str_radix` would otherwise take as a sign.
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Whether this terminal shows 24-bit colour, decided once.
fn truecolor() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let var = |k| std::env::var(k).ok();
        supports_truecolor(
            var("COLORTERM").as_deref(),
            var("TERM").as_deref(),
            var("TERM_PROGRAM").as_deref(),
        )
    })
}

/// `COLORTERM=truecolor` (or `24bit`) is the convention for saying so;
/// a few terminals that support it don't always set it, so they are
/// recognised by name. Anything else gets the 256-colour palette, which
/// every terminal blink is likely to meet can show.
fn supports_truecolor(
    colorterm: Option<&str>,
    term: Option<&str>,
    term_program: Option<&str>,
) -> bool {
    if matches!(colorterm, Some("truecolor" | "24bit")) {
        return true;
    }
    let term = term.unwrap_or("");
    if [
        "xterm-kitty",
        "alacritty",
        "foot",
        "wezterm",
        "xterm-ghostty",
    ]
    .iter()
    .any(|t| term.starts_with(t))
    {
        return true;
    }
    matches!(
        term_program,
        Some("WezTerm" | "iTerm.app" | "vscode" | "ghostty" | "Hyper" | "Tabby")
    )
}

/// The nearest xterm-256 index: the 6×6×6 cube or the 24-step grey ramp,
/// whichever is closer. The 16 system colours are skipped, since terminals
/// redefine them.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| -> usize {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] as i32 - v as i32).abs())
            .unwrap()
    };
    let dist = |(x, y, z): (u8, u8, u8)| -> i32 {
        let d = |p: u8, q: u8| (p as i32 - q as i32).pow(2);
        d(x, r) + d(y, g) + d(z, b)
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;

    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let k = ((avg - 8 + 5) / 10).clamp(0, 23);
    let v = (8 + 10 * k) as u8;
    let grey_idx = 232 + k as usize;

    if dist((v, v, v)) < dist(cube) {
        grey_idx as u8
    } else {
        cube_idx as u8
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// The built-ins, by key. The one list both lookup and listing read, so a
/// theme can't exist without being offered, or the other way round.
type Builtin = (&'static str, fn() -> Theme);

const BUILTINS: &[Builtin] = &[
    ("dracula", dracula),
    ("aura", aura),
    ("nord", nord),
    ("solarized-dark", solarized_dark),
    ("solarized-osaka", solarized_osaka),
    ("tokyo-night", tokyo_night),
    ("cyberpunk-neon", cyberpunk_neon),
];

fn builtin(key: &str) -> Option<Theme> {
    BUILTINS.iter().find(|(k, _)| *k == key).map(|(_, f)| f())
}

fn named(key: &str) -> (String, String) {
    (key.to_string(), key.to_string())
}

fn dracula() -> Theme {
    let (key, name) = named("dracula");
    Theme {
        key,
        name,
        bg: rgb(40, 42, 54),
        fg: rgb(248, 248, 242),
        dim: rgb(98, 114, 164),
        cursor_bg: rgb(68, 71, 90),
        border_active: rgb(189, 147, 249),
        border_inactive: rgb(68, 71, 90),
        accent: rgb(255, 121, 198),
        directory: rgb(139, 233, 253),
        image: rgb(255, 121, 198),
        selected: rgb(241, 250, 140),
        success: rgb(80, 250, 123),
        warning: rgb(255, 184, 108),
        error: rgb(255, 85, 85),
    }
}

fn aura() -> Theme {
    let (key, name) = named("aura");
    Theme {
        key,
        name,
        bg: rgb(21, 20, 28),
        fg: rgb(237, 236, 238),
        dim: rgb(110, 105, 137),
        cursor_bg: rgb(48, 45, 65),
        border_active: rgb(167, 139, 250),
        border_inactive: rgb(48, 45, 65),
        accent: rgb(255, 113, 192),
        directory: rgb(130, 224, 170),
        image: rgb(255, 192, 124),
        selected: rgb(255, 230, 113),
        success: rgb(97, 255, 202),
        warning: rgb(255, 192, 124),
        error: rgb(255, 113, 124),
    }
}

fn nord() -> Theme {
    let (key, name) = named("nord");
    Theme {
        key,
        name,
        bg: rgb(46, 52, 64),
        fg: rgb(216, 222, 233),
        dim: rgb(76, 86, 106),
        cursor_bg: rgb(59, 66, 82),
        border_active: rgb(136, 192, 208),
        border_inactive: rgb(67, 76, 94),
        accent: rgb(180, 142, 173),
        directory: rgb(143, 188, 187),
        image: rgb(180, 142, 173),
        selected: rgb(235, 203, 139),
        success: rgb(163, 190, 140),
        warning: rgb(208, 135, 112),
        error: rgb(191, 97, 106),
    }
}

fn solarized_dark() -> Theme {
    let (key, name) = named("solarized-dark");
    Theme {
        key,
        name,
        bg: rgb(0, 43, 54),
        // base0 for body text and base01 for secondary, as Solarized
        // specifies for a dark background (base2 is the light theme's
        // background).
        fg: rgb(131, 148, 150),
        dim: rgb(88, 110, 117),
        cursor_bg: rgb(7, 54, 66),
        border_active: rgb(38, 139, 210),
        border_inactive: rgb(7, 54, 66),
        accent: rgb(211, 54, 130),
        directory: rgb(42, 161, 152),
        image: rgb(211, 54, 130),
        selected: rgb(181, 137, 0),
        success: rgb(133, 153, 0),
        warning: rgb(203, 75, 22),
        error: rgb(220, 50, 47),
    }
}

fn solarized_osaka() -> Theme {
    // Darker, more saturated take on solarized.
    let (key, name) = named("solarized-osaka");
    Theme {
        key,
        name,
        bg: rgb(0, 28, 36),
        fg: rgb(238, 232, 213),
        dim: rgb(88, 110, 117),
        cursor_bg: rgb(7, 38, 48),
        border_active: rgb(108, 113, 196),
        border_inactive: rgb(7, 38, 48),
        accent: rgb(211, 54, 130),
        directory: rgb(42, 161, 152),
        image: rgb(238, 100, 158),
        selected: rgb(204, 161, 0),
        success: rgb(133, 153, 0),
        warning: rgb(203, 75, 22),
        error: rgb(220, 50, 47),
    }
}

fn tokyo_night() -> Theme {
    let (key, name) = named("tokyo-night");
    Theme {
        key,
        name,
        bg: rgb(26, 27, 38),
        fg: rgb(192, 202, 245),
        dim: rgb(86, 95, 137),
        cursor_bg: rgb(40, 42, 54),
        border_active: rgb(187, 154, 247),
        border_inactive: rgb(41, 46, 66),
        accent: rgb(247, 118, 142),
        directory: rgb(125, 207, 255),
        image: rgb(247, 118, 142),
        selected: rgb(224, 175, 104),
        success: rgb(158, 206, 106),
        warning: rgb(255, 158, 100),
        // Tokyo Night's red1; its red is already the accent.
        error: rgb(219, 75, 75),
    }
}

fn cyberpunk_neon() -> Theme {
    let (key, name) = named("cyberpunk-neon");
    Theme {
        key,
        name,
        bg: rgb(0, 10, 20),
        fg: rgb(213, 248, 247),
        dim: rgb(89, 104, 137),
        cursor_bg: rgb(20, 30, 50),
        border_active: rgb(255, 0, 255),
        border_inactive: rgb(45, 60, 95),
        accent: rgb(255, 0, 255),
        directory: rgb(0, 255, 255),
        image: rgb(255, 222, 0),
        selected: rgb(255, 255, 102),
        success: rgb(0, 255, 159),
        warning: rgb(255, 200, 0),
        error: rgb(255, 60, 100),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_valid() {
        assert_eq!(parse_color("#1a2b3c"), Some(Color::Rgb(0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_color("1a2b3c"), Some(Color::Rgb(0x1a, 0x2b, 0x3c)));
    }

    #[test]
    fn parse_color_rejects_non_ascii() {
        // Two 3-byte CJK chars → len()==6 but not ASCII; would panic before fix.
        assert_eq!(parse_color("一乙"), None);
    }

    #[test]
    fn parse_color_rejects_wrong_length() {
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("#1a2b3c4d"), None);
    }

    #[test]
    fn parse_color_rejects_invalid_hex() {
        assert_eq!(parse_color("#gggggg"), None);
    }

    // -- name vs. key ------------------------------------------------------
    //
    // A theme has two identifiers and they are not interchangeable: the *key*
    // that `load` accepts (a built-in name or a user file's stem, which is
    // what `list_all_names` yields), and `Theme.name`, the display label from
    // the optional `[theme] name` field. `cycle_theme` persists the key;
    // persisting the display name wrote a config value that resolved to
    // nothing, so the next launch silently fell back to dracula.

    #[test]
    fn every_listed_builtin_is_loadable_by_its_key() {
        // The invariant `cycle_theme` depends on: anything it can pick out of
        // the name list must come back from `load`, because that value is
        // what gets written to config.ini and re-loaded on the next launch.
        for name in Theme::list_builtin_names() {
            let theme =
                Theme::load(name).unwrap_or_else(|e| panic!("built-in {name} must load: {e}"));
            assert_eq!(
                &theme.name, name,
                "a built-in's display name doubles as its key",
            );
        }
    }

    #[test]
    fn user_theme_display_name_can_differ_from_its_key() {
        // Demonstrates the divergence the fix accounts for: the loaded theme
        // reports "My Cool Theme" while the only string that can load it
        // again is the filename stem.
        let _home = paths::test_home();
        write_user(
            "custom",
            "[theme]\nname = My Cool Theme\n\n[colors]\nbg=#000000\n",
        );

        let theme = Theme::load("custom").expect("theme must parse");
        assert_eq!(
            theme.name, "My Cool Theme",
            "display name comes from [theme]"
        );
        assert_eq!(theme.key, "custom", "the key is the file stem");
    }

    const FULL: &str = "[colors]\n\
         bg=#000000\nfg=#ffffff\ndim=#888888\ncursor_bg=#111111\n\
         border_active=#ff00ff\nborder_inactive=#222222\naccent=#ff0000\n\
         directory=#00ffff\nimage=#ffff00\nselected=#00ff00\n\
         success=#00ff00\nwarning=#ffaa00\nerror=#ff0000\n";

    fn write_user(stem: &str, body: &str) {
        std::fs::write(
            paths::themes_dir().unwrap().join(format!("{stem}.ini")),
            body,
        )
        .unwrap();
    }

    // -- parse_color strictness ---------------------------------------------

    /// `from_str_radix` takes a leading `+`, and `trim_start_matches` ate
    /// any number of `#`s, so typos loaded as some near-black colour.
    #[test]
    fn parse_color_rejects_signs_and_repeated_hashes() {
        assert_eq!(parse_color("+1+2+3"), None);
        assert_eq!(parse_color("#+1+2+3"), None);
        assert_eq!(parse_color("##1a2b3c"), None);
        assert_eq!(parse_color("###1a2b3c"), None);
        assert_eq!(parse_color(" #1A2B3C "), Some(Color::Rgb(0x1a, 0x2b, 0x3c)));
    }

    // -- built-ins ----------------------------------------------------------

    /// The other direction from `every_listed_builtin_is_loadable_by_its_key`:
    /// a built-in that exists but isn't listed can never be cycled to.
    #[test]
    fn every_builtin_is_listed_under_its_own_name() {
        assert_eq!(BUILTINS.len(), Theme::list_builtin_names().len());
        for (key, make) in BUILTINS {
            let t = make();
            assert_eq!(&t.name, key);
            assert_eq!(&t.key, key);
            assert!(Theme::list_builtin_names().contains(key));
        }
    }

    /// An error line has to stand out from an ordinary accent.
    #[test]
    fn no_builtin_draws_errors_in_its_accent_colour() {
        for (key, make) in BUILTINS {
            let t = make();
            assert_ne!(t.error, t.accent, "{key}");
        }
    }

    // -- one rule for names ---------------------------------------------------

    /// Built-ins were found ignoring case and with spaces for hyphens; user
    /// files only by their exact stem.
    #[test]
    fn user_themes_are_found_the_same_loose_way_as_builtins() {
        let _home = paths::test_home();
        write_user("Mine", FULL);

        for spelling in ["Mine", "mine", " MINE "] {
            let t = Theme::load(spelling).unwrap_or_else(|e| panic!("{spelling}: {e}"));
            assert_eq!(t.key, "mine", "{spelling}");
        }
        assert_eq!(Theme::load("Tokyo Night").unwrap().key, "tokyo-night");
    }

    #[test]
    fn a_user_file_with_a_space_is_listed_and_loaded_by_its_hyphenated_key() {
        let _home = paths::test_home();
        write_user("My Theme", FULL);

        assert!(Theme::list_all_names().contains(&"my-theme".to_string()));
        assert_eq!(Theme::load("my-theme").unwrap().key, "my-theme");
    }

    /// On a case-insensitive filesystem `Dracula.ini` *is* what `dracula`
    /// opens, so listing both showed the user file twice and the built-in
    /// never. Listed once, as a user theme, everywhere.
    #[test]
    fn a_user_file_shadowing_a_builtin_is_listed_once() {
        let _home = paths::test_home();
        write_user("Dracula", "[theme]\nname = mine\n[colors]\nbg=#010203\n");

        let names = Theme::list_all_names();
        assert_eq!(names.iter().filter(|n| *n == "dracula").count(), 1);
        assert_eq!(Theme::load("dracula").unwrap().name, "mine");
        let listing = Theme::list_detailed();
        let entry = listing.entries.iter().find(|e| e.key == "dracula").unwrap();
        assert!(entry.user);
    }

    /// A stem `validate_theme_name` refuses can never load, so it must not
    /// be offered.
    #[test]
    fn files_named_outside_the_rules_are_not_listed() {
        let _home = paths::test_home();
        write_user("x..y", FULL);
        if cfg!(unix) {
            write_user("a:b", FULL);
        }

        let names = Theme::list_all_names();
        assert!(
            !names.iter().any(|n| n.contains("..") || n.contains(':')),
            "{names:?}"
        );
        assert!(
            Theme::list_detailed()
                .invalid
                .contains(&"x..y.ini".to_string())
        );
    }

    // -- missing keys ---------------------------------------------------------

    /// Requiring every key meant a theme written before a colour was added
    /// stopped loading the day it was.
    #[test]
    fn missing_colours_come_from_dracula() {
        let _home = paths::test_home();
        write_user("partial", "[colors]\nbg=#010203\n");

        let t = Theme::load_rgb("partial").unwrap();
        let d = dracula();
        assert_eq!(t.bg, Color::Rgb(1, 2, 3));
        assert_eq!(t.fg, d.fg);
        assert_eq!(t.error, d.error);
    }

    #[test]
    fn a_bad_colour_is_still_an_error() {
        let _home = paths::test_home();
        write_user("typo", "[colors]\nbg=#01020\n");

        let e = Theme::load("typo").unwrap_err().to_string();
        assert!(e.contains("bad color for `bg`"), "{e}");
    }

    // -- startup fallback ---------------------------------------------------

    #[test]
    fn startup_fallback_says_why_the_theme_failed() {
        let _home = paths::test_home();
        write_user("typo", "[colors]\nbg=nope\n");

        let (t, warning) = Theme::load_or_fallback("typo");
        assert_eq!(t.key, "dracula");
        let w = warning.unwrap();
        assert!(w.contains("bad color for `bg`"), "{w}");
        assert!(!w.contains("not found"), "{w}");

        let (_, warning) = Theme::load_or_fallback("long-gone");
        assert!(warning.unwrap().contains("not found"));

        let (t, warning) = Theme::load_or_fallback("nord");
        assert_eq!((t.key.as_str(), warning), ("nord", None));
    }

    // -- 256-colour fallback ------------------------------------------------

    #[test]
    fn truecolor_is_assumed_only_when_the_terminal_says_so() {
        let yes = |c, t, p| supports_truecolor(c, t, p);
        assert!(yes(Some("truecolor"), Some("xterm-256color"), None));
        assert!(yes(Some("24bit"), None, None));
        assert!(yes(None, Some("xterm-kitty"), None));
        assert!(yes(None, Some("alacritty"), None));
        assert!(yes(None, Some("xterm-256color"), Some("WezTerm")));
        assert!(yes(None, Some("xterm-256color"), Some("iTerm.app")));
        assert!(!yes(None, Some("xterm-256color"), Some("Apple_Terminal")));
        assert!(!yes(None, Some("linux"), None));
        assert!(!yes(None, None, None));
    }

    #[test]
    fn rgb_maps_to_the_nearest_xterm_256_colour() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(0, 255, 255), 51);
        // Greys go to the 24-step ramp when it is closer than the cube.
        assert_eq!(rgb_to_256(128, 128, 128), 244);
        assert_eq!(rgb_to_256(40, 42, 54), 236);
    }

    #[test]
    fn a_downgraded_theme_has_no_rgb_left() {
        let t = dracula().into_256();
        for c in [
            t.bg,
            t.fg,
            t.dim,
            t.cursor_bg,
            t.border_active,
            t.border_inactive,
            t.accent,
            t.directory,
            t.image,
            t.selected,
            t.success,
            t.warning,
            t.error,
        ] {
            assert!(matches!(c, Color::Indexed(_)), "{c:?}");
        }
        assert_eq!(t.key, "dracula");
    }
}
