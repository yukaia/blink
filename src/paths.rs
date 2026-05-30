//! Platform-specific paths for config, sessions, and themes.
//!
//! - Linux / macOS: `~/.config/blink/` (or `$XDG_CONFIG_HOME/blink/` if set)
//! - Windows: `%USERPROFILE%\Documents\blink\`

use std::path::{Path, PathBuf};
use std::{env, fs};

use crate::error::{BlinkError, Result};

const APP_DIR_NAME: &str = "blink";

/// Returns the root data directory for blink, creating it if needed.
pub fn root_dir() -> Result<PathBuf> {
    let dir = base_dir()?;
    create_app_dir(&dir)?;
    Ok(dir)
}

/// Path to the global `config.ini`. Does not create the file.
pub fn config_file() -> Result<PathBuf> {
    Ok(root_dir()?.join("config.ini"))
}

/// Directory holding per-session `.ini` files. Created if missing.
pub fn sessions_dir() -> Result<PathBuf> {
    let dir = root_dir()?.join("sessions");
    create_app_dir(&dir)?;
    Ok(dir)
}

/// Directory holding user-supplied theme `.ini` files. Created if missing.
pub fn themes_dir() -> Result<PathBuf> {
    let dir = root_dir()?.join("themes");
    create_app_dir(&dir)?;
    Ok(dir)
}

/// Directory holding walk checkpoint `.json` files. Created if missing.
///
/// One file per (session, direction) pair; overwritten on each new walk so
/// stale files don't accumulate.
pub fn checkpoints_dir() -> Result<PathBuf> {
    let dir = root_dir()?.join("checkpoints");
    create_app_dir(&dir)?;
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn base_dir() -> Result<PathBuf> {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty() {
            let p = PathBuf::from(&xdg);
            if !p.is_absolute() {
                return Err(BlinkError::config(
                    "XDG_CONFIG_HOME must be an absolute path",
                ));
            }
            return Ok(p.join(APP_DIR_NAME));
        }
    let home = env::var("HOME").map_err(|_| BlinkError::config("$HOME is not set"))?;
    let home_path = PathBuf::from(&home);
    if !home_path.is_absolute() {
        return Err(BlinkError::config("$HOME must be an absolute path"));
    }
    Ok(home_path.join(".config").join(APP_DIR_NAME))
}

#[cfg(target_os = "macos")]
fn base_dir() -> Result<PathBuf> {
    // macOS convention is `$HOME/Library/Application Support/<App>`. Honour
    // XDG_CONFIG_HOME if the user has explicitly set it (some cross-platform
    // Mac users prefer the XDG layout), otherwise fall back to the standard
    // location — which is what the README documents.
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            let p = PathBuf::from(&xdg);
            if !p.is_absolute() {
                return Err(BlinkError::config(
                    "XDG_CONFIG_HOME must be an absolute path",
                ));
            }
            return Ok(p.join(APP_DIR_NAME));
        }
    }
    let home = env::var("HOME").map_err(|_| BlinkError::config("$HOME is not set"))?;
    let home_path = PathBuf::from(&home);
    if !home_path.is_absolute() {
        return Err(BlinkError::config("$HOME must be an absolute path"));
    }
    Ok(home_path
        .join("Library")
        .join("Application Support")
        .join(APP_DIR_NAME))
}

#[cfg(target_os = "windows")]
fn base_dir() -> Result<PathBuf> {
    let user_profile = env::var("USERPROFILE")
        .map_err(|_| BlinkError::config("%USERPROFILE% is not set"))?;
    let profile_path = PathBuf::from(&user_profile);
    if !profile_path.is_absolute() {
        return Err(BlinkError::config("%USERPROFILE% must be an absolute path"));
    }
    Ok(profile_path.join("Documents").join(APP_DIR_NAME))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn base_dir() -> Result<PathBuf> {
    if let Some(proj) = directories::ProjectDirs::from("", "", APP_DIR_NAME) {
        return Ok(proj.config_dir().to_path_buf());
    }
    Ok(env::current_dir()?.join(APP_DIR_NAME))
}

/// Create `path` as an application-owned directory with restricted permissions.
///
/// On Unix the directory is created with mode 0700 (owner read/write/execute
/// only) so that session configs, known_hosts, and checkpoint files are not
/// world-readable. Parent directories are created with default permissions
/// (they may already exist and be shared with other applications).
///
/// On non-Unix platforms `fs::create_dir_all` is used unchanged.
fn create_app_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        // Create any missing parents with default permissions — they may be
        // shared paths like ~/.config that other apps rely on.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Create the target directory itself with 0700. Ignore AlreadyExists
        // so this is idempotent on repeated calls.
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(BlinkError::from(e)),
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
        Ok(())
    }
}

/// Fsync the parent directory of `path` so that a recent rename into it is
/// crash-durable.
///
/// On Linux, ext4/xfs/btrfs all require the parent directory's inode to be
/// synced for a `rename()` to survive a power loss — without it, the journal
/// can roll back the rename even though it returned Ok.
///
/// On Windows the call is a no-op: opening a directory handle works but
/// `sync_all()` on it isn't part of the durability contract there; the
/// filesystem journals rename through its own ordering rules.
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            // Empty parent means a bare filename in the cwd — nothing useful
            // to sync.
            if !parent.as_os_str().is_empty() {
                std::fs::File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Default local working directory: the user's home dir.
///
/// Falls back to the filesystem root rather than a relative path so that the
/// TUI always has an absolute, navigable starting point even if the process
/// working directory is unavailable.
pub fn default_local_dir() -> PathBuf {
    if let Some(home) = directories::UserDirs::new() {
        return home.home_dir().to_path_buf();
    }
    env::current_dir().unwrap_or_else(|_| {
        if cfg!(windows) {
            PathBuf::from("C:\\")
        } else {
            PathBuf::from("/")
        }
    })
}
