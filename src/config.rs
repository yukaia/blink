//! Global configuration loaded from `config.ini`.
//!
//! ```ini
//! [general]
//! theme = dracula
//! parallel_downloads = 2
//! confirm_quit = true
//!
//! [terminal]
//! image_preview = auto    ; auto | kitty | sixel | iterm2 | none
//! ```

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
            },
        }
    }
}

impl Config {
    /// Load `config.ini` from the standard location. If the file does not
    /// exist, write out a default and return it — first-run UX, so the user
    /// has a documented file to edit. Save failures (read-only fs, perms)
    /// fall through to in-memory defaults rather than refusing to launch.
    pub fn load() -> Result<Self> {
        let path = paths::config_file()?;
        match Self::load_from(&path) {
            Ok(cfg) => Ok(cfg),
            Err(BlinkError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // First run: write defaults so the user has a file to edit.
                let cfg = Self::default();
                if let Err(e) = cfg.save() {
                    tracing::warn!(?path, "could not write default config: {e}");
                }
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        // Enforce a size limit before reading so a large or malicious file
        // doesn't exhaust memory.
        let meta = fs::metadata(path)?;
        if meta.len() > MAX_CONFIG_BYTES {
            return Err(BlinkError::config(format!(
                "config file is too large ({} bytes, limit is {MAX_CONFIG_BYTES})",
                meta.len()
            )));
        }

        let raw = fs::read_to_string(path)?;
        let ini = Ini::load_from_str(&raw)
            .map_err(|e| BlinkError::config(format!("{}: {e}", path.display())))?;
        let mut cfg = Self::default();

        if let Some(s) = ini.section(Some("general")) {
            if let Some(v) = s.get("theme") {
                let name = v.trim();
                validate_theme_name(name)?;
                cfg.general.theme = name.to_string();
            }
            if let Some(v) = s.get("parallel_downloads") {
                let n: u8 = v.trim().parse().map_err(|_| {
                    BlinkError::config(format!(
                        "parallel_downloads must be an integer between 1 and {MAX_PARALLEL}: {v}"
                    ))
                })?;
                if n == 0 {
                    tracing::warn!(
                        "config: parallel_downloads = 0 is out of range; \
                         clamped to 1"
                    );
                }
                cfg.general.parallel_downloads = n.clamp(1, MAX_PARALLEL);
            }
            if let Some(v) = s.get("confirm_quit") {
                cfg.general.confirm_quit = parse_bool(v)?;
            }
        }
        if let Some(s) = ini.section(Some("terminal")) {
            if let Some(v) = s.get("image_preview") {
                cfg.terminal.image_preview = match v.trim().to_ascii_lowercase().as_str() {
                    "auto" => ImagePreviewMode::Auto,
                    "kitty" => ImagePreviewMode::Kitty,
                    "sixel" => ImagePreviewMode::Sixel,
                    "iterm2" => ImagePreviewMode::Iterm2,
                    "none" | "off" | "false" => ImagePreviewMode::None,
                    _ => {
                        return Err(BlinkError::config(
                            "image_preview must be one of: auto, kitty, sixel, iterm2, none",
                        ))
                    }
                };
            }
        }
        Ok(cfg)
    }

    /// Serialize and write `config.ini` atomically (write to `.tmp`, rename).
    pub fn save(&self) -> Result<()> {
        let path = paths::config_file()?;
        let tmp = path.with_extension("tmp");

        let mut ini = Ini::new();
        ini.with_section(Some("general"))
            .set("theme", &self.general.theme)
            .set(
                "parallel_downloads",
                self.general.parallel_downloads.to_string(),
            )
            .set("confirm_quit", self.general.confirm_quit.to_string());
        ini.with_section(Some("terminal")).set(
            "image_preview",
            match self.terminal.image_preview {
                ImagePreviewMode::Auto => "auto",
                ImagePreviewMode::Kitty => "kitty",
                ImagePreviewMode::Sixel => "sixel",
                ImagePreviewMode::Iterm2 => "iterm2",
                ImagePreviewMode::None => "none",
            },
        );

        ini.write_to_file(&tmp)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
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
    // Reject path separators and null bytes outright.
    if name.contains(['/', '\\', '\0']) {
        return Err(BlinkError::config(
            "theme name must not contain path separators",
        ));
    }
    // Reject `..` anywhere in the name to block traversal via dots.
    if name.split('/').any(|c| c == "..") || name.contains("..") {
        return Err(BlinkError::config(
            "theme name must not contain '..'",
        ));
    }
    Ok(())
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(BlinkError::config(
            "boolean value must be one of: true, false, yes, no, on, off, 1, 0",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // parse_bool
    #[test]
    fn parse_bool_truthy_values() {
        for s in &["1", "true", "yes", "on", "TRUE", "Yes", " on "] {
            assert_eq!(parse_bool(s).unwrap(), true, "expected true for {s:?}");
        }
    }

    #[test]
    fn parse_bool_falsy_values() {
        for s in &["0", "false", "no", "off", "FALSE", "No", " off "] {
            assert_eq!(parse_bool(s).unwrap(), false, "expected false for {s:?}");
        }
    }

    #[test]
    fn parse_bool_invalid_errors() {
        assert!(parse_bool("maybe").is_err());
        assert!(parse_bool("").is_err());
        assert!(parse_bool("2").is_err());
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

    #[test]
    fn theme_name_null_byte_errors() {
        assert!(validate_theme_name("evil\0theme").is_err());
    }
}
