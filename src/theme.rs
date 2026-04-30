//! Themes: a small palette of named colors used everywhere the TUI renders.
//!
//! Eight built-in themes ship with the binary. Users can add more by dropping
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
//! error           = #f7768e
//! ```

use std::fs;
use std::io::Read;
use std::path::Path;

use ini::Ini;
use ratatui::style::Color;

use crate::config;
use crate::error::{self, BlinkError, Result};
use crate::paths;

/// Maximum theme file size accepted on load (64 KiB).
const MAX_THEME_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Theme {
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

impl Theme {
    /// Resolve a theme by name. User themes (in the themes dir) shadow built-ins.
    pub fn load(name: &str) -> Result<Self> {
        // Validate before constructing any path — load() may be called from
        // contexts that haven't gone through config/session validation.
        config::validate_theme_name(name)?;

        let user_path = paths::themes_dir()?.join(format!("{name}.ini"));

        // Attempt to load the user theme, treating NotFound as "fall through
        // to built-ins". This eliminates the exists()-then-open() TOCTOU race.
        match Self::load_from(&user_path) {
            Ok(theme) => return Ok(theme),
            Err(BlinkError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        builtin(name).ok_or_else(|| BlinkError::theme_not_found(name))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        // Enforce a size limit before parsing so a large or malicious theme
        // file cannot exhaust memory.
        let file = fs::File::open(path)?;
        let mut raw = String::new();
        file.take(MAX_THEME_BYTES + 1).read_to_string(&mut raw)?;
        if raw.len() as u64 > MAX_THEME_BYTES {
            return Err(BlinkError::config(format!(
                "theme file is too large (limit is {MAX_THEME_BYTES} bytes)"
            )));
        }

        let ini = Ini::load_from_str(&raw)
            .map_err(|e| BlinkError::config(format!("{}: {e}", path.display())))?;
        let s = ini.section(Some("colors")).ok_or_else(|| {
            BlinkError::config(format!("{}: missing [colors] section", path.display()))
        })?;

        // Sanitize the display name so ANSI sequences from a crafted
        // [theme] name field don't reach the TUI status bar.
        let name = ini
            .section(Some("theme"))
            .and_then(|t| t.get("name"))
            .map(|s| error::sanitize(s.to_string()))
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| error::sanitize(s.to_string()))
            })
            .unwrap_or_else(|| "custom".to_string());

        let g = |key: &str| -> Result<Color> {
            let v = s.get(key).ok_or_else(|| {
                BlinkError::config(format!("{}: missing color `{key}`", path.display()))
            })?;
            parse_color(v).ok_or_else(|| {
                BlinkError::config(format!("{}: bad color for `{key}`", path.display()))
            })
        };

        Ok(Self {
            name,
            bg: g("bg")?,
            fg: g("fg")?,
            dim: g("dim")?,
            cursor_bg: g("cursor_bg")?,
            border_active: g("border_active")?,
            border_inactive: g("border_inactive")?,
            accent: g("accent")?,
            directory: g("directory")?,
            image: g("image")?,
            selected: g("selected")?,
            success: g("success")?,
            warning: g("warning")?,
            error: g("error")?,
        })
    }

    pub fn list_builtin_names() -> &'static [&'static str] {
        BUILTIN_NAMES
    }

    /// Every theme name the user could realistically pick, deduplicated and
    /// sorted: the built-ins plus any `<n>.ini` files in the user's
    /// themes directory. Used for the in-app cycle hotkey.
    ///
    /// Bad theme files (unreadable, missing `[colors]`, etc.) are silently
    /// skipped — they'll never load anyway, so listing them would just
    /// dead-end the cycle.
    pub fn list_all_names() -> Vec<String> {
        use std::collections::BTreeSet;
        let mut set: BTreeSet<String> =
            BUILTIN_NAMES.iter().map(|s| s.to_string()).collect();
        if let Ok(dir) = paths::themes_dir() {
            if let Ok(read) = std::fs::read_dir(&dir) {
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("ini") {
                        continue;
                    }
                    // Quick validity probe: only list it if it actually parses.
                    if Self::load_from(&path).is_ok() {
                        if let Some(stem) =
                            path.file_stem().and_then(|s| s.to_str())
                        {
                            set.insert(stem.to_string());
                        }
                    }
                }
            }
        }
        set.into_iter().collect()
    }
}

/// Parse a CSS-style hex color string (`#rrggbb` or `rrggbb`).
///
/// Rejects non-ASCII input before byte-slicing to prevent a panic at
/// char-boundary checks. Multi-byte UTF-8 chars can satisfy `len() == 6`
/// while having char boundaries that don't align with the 0/2/4/6 offsets.
fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

const BUILTIN_NAMES: &[&str] = &[
    "dracula",
    "aura",
    "nord",
    "solarized-dark",
    "solarized-osaka",
    "tokyo-night",
    "cyberpunk-neon",
];

fn builtin(name: &str) -> Option<Theme> {
    let n = name.trim().to_ascii_lowercase().replace(' ', "-");
    Some(match n.as_str() {
        "dracula" => Theme {
            name: "dracula".into(),
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
        },
        "aura" => Theme {
            name: "aura".into(),
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
        },
        "nord" => Theme {
            name: "nord".into(),
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
        },
        "solarized-dark" => Theme {
            name: "solarized-dark".into(),
            bg: rgb(0, 43, 54),
            fg: rgb(238, 232, 213),
            dim: rgb(101, 123, 131),
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
        },
        "solarized-osaka" => Theme {
            // Darker, more saturated take on solarized.
            name: "solarized-osaka".into(),
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
        },
        "tokyo-night" => Theme {
            name: "tokyo-night".into(),
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
            error: rgb(247, 118, 142),
        },
        "cyberpunk-neon" => Theme {
            name: "cyberpunk-neon".into(),
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
        },
        _ => return None,
    })
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
}
