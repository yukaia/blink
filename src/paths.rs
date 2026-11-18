//! Platform-specific paths for everything blink keeps: `config.ini`,
//! `known_hosts`, and the `sessions/`, `themes/` and `checkpoints/` folders.
//!
//! - Linux and other Unix (the BSDs): `~/.config/blink/` (or
//!   `$XDG_CONFIG_HOME/blink/` if set)
//! - macOS: `~/Library/Application Support/blink/` (or
//!   `$XDG_CONFIG_HOME/blink/` if set)
//! - Windows: `%APPDATA%\blink\`
//!
//! The location must resolve to an absolute path; anything else is an
//! error rather than a guess. On Unix the folders are kept `0700` and the
//! files blink writes `0600`.

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
/// One file per (session, direction) pair. A second batch of the same
/// direction *appends* to the existing file rather than replacing it —
/// overwriting used to destroy the plan of a batch that was still running,
/// taking its resumability with it. A file is removed once nothing in it
/// still needs to run.
pub fn checkpoints_dir() -> Result<PathBuf> {
    let dir = root_dir()?.join("checkpoints");
    create_app_dir(&dir)?;
    Ok(dir)
}

/// Root of the application's data directory.
///
/// Under test this never resolves to the user's real directory — see
/// `test_home`. That guarantee rests on `cfg(test)` being set for every
/// test that runs, which holds only because this crate has no `[lib]`
/// target and every test lives inline behind `#[cfg(test)] mod tests`; a
/// `[lib]` target plus a `tests/` integration-test directory would compile
/// this crate as a normal (non-`cfg(test)`) dependency and reach
/// `real_base_dir` instead.
fn base_dir() -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(test_home::current())
    }
    #[cfg(not(test))]
    {
        real_base_dir()
    }
}

/// A private config home for one test, active until the guard drops.
///
/// Tests that write through `paths` need each other's writes kept apart:
/// without this they share one directory and race, which is why the
/// `discard` removal-failure property could not be tested before.
///
/// Prefer parameter injection (`load_from(&Path)`, as in `config.rs`,
/// `session.rs`, and `theme.rs`) when the code under test accepts a path
/// directly; reach for `test_home()` only when the call goes through
/// `paths` and so has no path parameter to inject into.
#[cfg(test)]
pub(crate) fn test_home() -> TestHome {
    test_home::acquire()
}

#[cfg(test)]
pub(crate) use test_home::TestHome;

#[cfg(test)]
mod test_home {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The config home for the current test.
    //
    // Thread-local because the test harness runs each `#[test]` on its own
    // thread, and every async test in this crate is `#[tokio::test]` with
    // the default current-thread flavor, so tasks it spawns stay on that
    // thread. A `#[tokio::test(flavor = "multi_thread")]` that touched
    // `paths` would see the shared home below instead of its own — read
    // this comment before adding one.
    thread_local! {
        static OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// The home in force on this thread: a guard's private directory if one
    /// is held, otherwise the shared per-process scratch directory.
    ///
    /// The shared fallback is what makes the real directory unreachable
    /// rather than merely avoidable: a test written later that calls
    /// `Session::save()` without thinking about isolation lands here, not in
    /// the user's config.
    pub fn current() -> PathBuf {
        OVERRIDE.with(|o| o.borrow().clone()).unwrap_or_else(shared)
    }

    /// Create `dir`, refusing to adopt anything already at that path.
    ///
    /// [`super::create_app_dir`] deliberately tolerates `AlreadyExists`,
    /// which is right for `~/.config/blink`: only the user can write its
    /// parent, so anything already there is the user's own. Here the parent
    /// is world-writable `temp_dir()`, where that same tolerance is exactly
    /// what would let a local attacker leave a symlink behind and have every
    /// unguarded test write through it into the victim's real config.
    ///
    /// `mkdir(2)` is atomic and fails on anything already at the path,
    /// symlinks included. That is what makes the sweep in [`claim_dir`] safe
    /// rather than merely likely to work: an attacker who replants in the
    /// window between the two calls loses the path instead of winning it.
    pub fn create_exclusive(dir: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(dir)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir(dir)
        }
    }

    /// Clear `dir` of whatever is there and claim it as ours.
    ///
    /// The sweep handles the ordinary case — a previous run with the same
    /// pid, or an attacker's pre-planted link. `remove_dir_all` on a symlink
    /// removes the link itself and leaves its target alone, so sweeping
    /// cannot be turned into a deletion primitive. The exclusive create then
    /// refuses the path outright if anything reappears.
    pub fn claim_dir(dir: &Path) -> std::io::Result<()> {
        let _ = std::fs::remove_dir_all(dir);
        create_exclusive(dir)
    }

    fn shared() -> PathBuf {
        static SHARED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| {
                let dir = std::env::temp_dir().join(format!("blink-test-{}", std::process::id()));
                claim_dir(&dir).unwrap_or_else(|e| {
                    panic!(
                        "refusing to run: cannot claim a private config home at {} — \
                         something else holds that path: {e}",
                        dir.display(),
                    )
                });
                dir
            })
            .clone()
    }

    pub struct TestHome {
        dir: PathBuf,
        // The override this guard displaced, restored when this guard
        // drops. Without this, a nested guard's Drop would clear the
        // override to `None` unconditionally, silently demoting a
        // still-alive outer guard to the shared per-process directory.
        previous: Option<PathBuf>,
        // Makes `TestHome` `!Send`. The override it manipulates is
        // thread-local, so a guard dropped on a thread other than the one
        // that acquired it would restore *that* thread's override, not its
        // own — silently demoting the acquiring thread to the shared
        // per-process directory while its own override still points at a
        // tree `drop` just deleted. That's the same failure the nested-guard
        // fix above closes, reachable a different way, so it is made a
        // compile error instead of a runtime one.
        _not_send: std::marker::PhantomData<*const ()>,
    }

    impl TestHome {
        pub fn path(&self) -> &Path {
            &self.dir
        }
    }

    pub fn acquire() -> TestHome {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("blink-test-{}-{n}", std::process::id()));
        // Claim the directory eagerly. Leaving it to the first `root_dir()`
        // call would mean `create_app_dir` creating it, and its
        // `AlreadyExists` tolerance would adopt anything planted in the
        // meantime — a wider window than the shared home's, since the gap
        // spans everything the test does before its first path resolution.
        claim_dir(&dir).unwrap_or_else(|e| {
            panic!(
                "refusing to run this test: cannot claim a private config home \
                 at {} — something else holds that path: {e}",
                dir.display(),
            )
        });
        let previous = OVERRIDE.with(|o| o.borrow_mut().replace(dir.clone()));
        TestHome {
            dir,
            previous,
            _not_send: std::marker::PhantomData,
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            // Restore the previous override *before* removing the tree: if
            // the removal fails, a later call on this thread must not keep
            // resolving to a directory we just tried to delete.
            OVERRIDE.with(|o| *o.borrow_mut() = self.previous.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Validate one environment value as an absolute path.
///
/// A relative config home is refused rather than resolved against the
/// current directory: where the config lands would then depend on where
/// blink happened to be started from.
///
/// For `XDG_CONFIG_HOME` this departs from the XDG spec on purpose: the
/// spec says to ignore a relative value, but ignoring it would put the
/// config somewhere other than where the user asked, silently. An error
/// says what's wrong.
#[cfg(any(unix, target_os = "windows"))]
fn require_absolute(value: &str, var: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(BlinkError::config(format!(
            "{var} must be an absolute path"
        )));
    }
    Ok(path)
}

/// Resolve the base directory from the two environment values that decide
/// it, taking them as arguments so the rules can be tested without mutating
/// the process environment.
///
/// An empty `XDG_CONFIG_HOME` counts as unset, matching the XDG spec and
/// what every other XDG-aware tool does with it.
///
/// Used on every Unix but macOS: the BSDs follow the same XDG layout.
#[cfg(all(unix, not(target_os = "macos")))]
fn base_dir_from(xdg: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Ok(require_absolute(xdg, "XDG_CONFIG_HOME")?.join(APP_DIR_NAME));
    }
    let home = home.ok_or_else(|| BlinkError::config("$HOME is not set"))?;
    Ok(require_absolute(home, "$HOME")?
        .join(".config")
        .join(APP_DIR_NAME))
}

#[cfg(all(unix, not(target_os = "macos")))]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
    let xdg = env::var("XDG_CONFIG_HOME").ok();
    let home = env::var("HOME").ok();
    base_dir_from(xdg.as_deref(), home.as_deref())
}

/// macOS convention is `$HOME/Library/Application Support/<App>`. Honour
/// `XDG_CONFIG_HOME` if the user has explicitly set it (some cross-platform
/// Mac users prefer the XDG layout), otherwise fall back to the standard
/// location — which is what the README documents.
#[cfg(target_os = "macos")]
fn base_dir_from(xdg: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Ok(require_absolute(xdg, "XDG_CONFIG_HOME")?.join(APP_DIR_NAME));
    }
    let home = home.ok_or_else(|| BlinkError::config("$HOME is not set"))?;
    Ok(require_absolute(home, "$HOME")?
        .join("Library")
        .join("Application Support")
        .join(APP_DIR_NAME))
}

#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
    let xdg = env::var("XDG_CONFIG_HOME").ok();
    let home = env::var("HOME").ok();
    base_dir_from(xdg.as_deref(), home.as_deref())
}

/// `%APPDATA%\blink`: the roaming application-data folder, where Windows
/// programs keep their settings. The config used to live in the user's
/// Documents folder, which is for the user's own files, and which OneDrive
/// often syncs, carrying `known_hosts` and session files with it.
#[cfg(target_os = "windows")]
fn base_dir_from(app_data: Option<&str>) -> Result<PathBuf> {
    let app_data = app_data.ok_or_else(|| BlinkError::config("%APPDATA% is not set"))?;
    Ok(require_absolute(app_data, "%APPDATA%")?.join(APP_DIR_NAME))
}

#[cfg(target_os = "windows")]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
    let app_data = env::var("APPDATA").ok();
    base_dir_from(app_data.as_deref())
}

/// Anywhere else there is no known place for a config, and guessing one
/// (the current directory, say) would scatter it by where blink was run.
#[cfg(not(any(unix, target_os = "windows")))]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
    Err(BlinkError::config(
        "no config directory is known for this platform",
    ))
}

/// Things worth telling the user that happen where there's no log to tell
/// them in — a config folder made private, say. Drained by
/// [`take_notices`]: by the TUI on every pass of its event loop, so one
/// raised mid-session (the first `t` creating `themes/`) shows up at once.
///
/// Per-thread under test, like the test home, so one test draining the
/// list can't take another's notices.
#[cfg(not(test))]
static NOTICES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
thread_local! {
    static NOTICES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn notice(msg: String) {
    #[cfg(not(test))]
    if let Ok(mut n) = NOTICES.lock() {
        n.push(msg);
    }
    #[cfg(test)]
    NOTICES.with(|n| n.borrow_mut().push(msg));
}

/// Everything [`notice`]d so far, oldest first; clears the list.
pub fn take_notices() -> Vec<String> {
    #[cfg(not(test))]
    {
        NOTICES
            .lock()
            .map(|mut n| std::mem::take(&mut *n))
            .unwrap_or_default()
    }
    #[cfg(test)]
    {
        NOTICES.with(|n| std::mem::take(&mut *n.borrow_mut()))
    }
}

/// Create `path` as an application-owned directory with restricted permissions.
///
/// On Unix the directory is created with mode 0700 (owner read/write/execute
/// only) so that session configs, known_hosts, and checkpoint files are not
/// readable by other users. Parent directories are created with default
/// permissions (they may already exist and be shared with other
/// applications).
///
/// A directory that already exists but is open to others — made by hand, by
/// a dotfiles sync, restored from a backup — is tightened to 0700 and the
/// change noted for the log. If that fails (it isn't ours to change), that
/// is noted instead. Something at `path` that isn't a directory is an error
/// here, rather than a confusing "Not a directory" from whatever opens a
/// file inside it later.
///
/// On non-Unix platforms `fs::create_dir_all` is used unchanged.
fn create_app_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        // Create any missing parents with default permissions — they may be
        // shared paths like ~/.config that other apps rely on.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(BlinkError::from(e)),
        }

        // Already there. Follows a symlink: the user may keep the folder
        // elsewhere, and only they can write its parent.
        let meta = fs::metadata(path)?;
        if !meta.is_dir() {
            return Err(BlinkError::config(format!(
                "{} exists but is not a directory",
                path.display()
            )));
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            match fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
                Ok(()) => notice(format!(
                    "{} was readable by other users (mode {mode:o}); made it private (700)",
                    path.display()
                )),
                Err(e) => notice(format!(
                    "{} is readable by other users (mode {mode:o}) and could not be \
                     made private: {e}",
                    path.display()
                )),
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)?;
        if !path.is_dir() {
            return Err(BlinkError::config(format!(
                "{} exists but is not a directory",
                path.display()
            )));
        }
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

/// Longest file name every platform blink runs on accepts: 255 bytes on
/// Linux and macOS, 255 UTF-16 units on Windows (never more than the UTF-8
/// byte count).
pub const MAX_FILE_NAME_BYTES: usize = 255;

/// A file name for a user-chosen `name`: `{stem}-{hash8}{suffix}`, where
/// `hash8` is the first four bytes of `hash` (a SHA-256 digest) in hex.
///
/// The stem is `name` with path separators, shell-special characters and
/// control characters (C0, DEL and C1) mapped to `_`, the same on every
/// platform: Windows refuses control characters in a name, and elsewhere a
/// newline or escape in a file name is only trouble for whoever lists the
/// folder. It is cut on a char boundary to keep the whole within
/// [`MAX_FILE_NAME_BYTES`]. A longer name could never have been written, so
/// cutting moves no existing file, and `hash`, taken over the full name,
/// keeps cut names (and names that map to the same stem) apart.
pub fn hashed_file_name(name: &str, hash: &[u8; 32], suffix: &str) -> String {
    use std::fmt::Write as _;

    let mut tail = String::from("-");
    for b in &hash[..4] {
        let _ = write!(&mut tail, "{b:02x}");
    }
    tail.push_str(suffix);

    let budget = MAX_FILE_NAME_BYTES - tail.len();
    let mut stem = String::with_capacity(name.len().min(budget));
    for c in name.chars() {
        let c = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
            c if c.is_control() => '_',
            c => c,
        };
        if stem.len() + c.len_utf8() > budget {
            break;
        }
        stem.push(c);
    }
    stem + &tail
}

/// Atomically and durably replace `path` with what `write` puts in a file:
/// write a temp file beside it, `sync_all`, rename it over `path`, then
/// [`sync_parent_dir`]. Without the syncs, a power loss between the rename
/// and the journal commit can leave a zero-byte file or undo the rename.
///
/// The temp file's name is unique to this process and call, and it is
/// opened create-new. A fixed `<name>.tmp` was shared by every blink running
/// at once: two saving the same file wrote into one temp file, and could
/// rename a mix of both into place. Create-new also means whatever already
/// sits at the name, a link included, is never written through. On failure
/// the temp file is removed, and `path` is left as it was.
///
/// On Unix the file is created `0600`. It used to take the umask's default
/// (typically `0644`), private only for as long as its folder was — and
/// since every save is a new file, a `chmod 600` lasted until the next one.
pub fn atomic_write(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = temp_path(
        path,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    );
    atomic_write_via(path, &tmp, write, sync_parent_dir)
}

/// The temp name for one [`atomic_write`] of `path`: `.{name}.{pid}.{n}.tmp`,
/// with `name` cut so the whole stays within [`MAX_FILE_NAME_BYTES`] — a
/// `path` whose own name is near the limit is still writable. The pid and
/// counter keep cut names unique.
fn temp_path(path: &Path, pid: u32, n: u64) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tail = format!(".{pid}.{n}.tmp");
    let budget = MAX_FILE_NAME_BYTES - 1 - tail.len();
    let mut cut = name.len().min(budget);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    path.with_file_name(format!(".{}{tail}", &name[..cut]))
}

/// Whether `name` is a temp file [`atomic_write`] would make:
/// `.{name}.{pid}.{n}.tmp`.
pub fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".tmp")) else {
        return false;
    };
    let mut parts = rest.rsplitn(3, '.');
    let digits =
        |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    digits(parts.next()) && digits(parts.next()) && parts.next().is_some_and(|n| !n.is_empty())
}

/// [`atomic_write`] through a given temp path and directory sync, so tests
/// can plant something at the temp name and make the sync fail.
fn atomic_write_via(
    path: &Path,
    tmp: &Path,
    write: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
    sync_dir: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp)?;
    let written = write(&mut f).and_then(|()| f.sync_all()).and_then(|()| {
        drop(f);
        fs::rename(tmp, path)
    });
    if let Err(e) = written {
        let _ = fs::remove_file(tmp);
        return Err(e.into());
    }
    // The new file is in place: from here, failing would tell the caller a
    // save didn't happen when it did. Some filesystems (FUSE, some network
    // mounts) refuse to sync a directory at all, and every save would then
    // report an error. Durability is the most that's lost, so say so in
    // the debug log and carry on.
    if let Err(e) = sync_dir(path) {
        tracing::warn!(?path, "saved, but syncing its folder failed: {e}");
    }
    Ok(())
}

/// How old a leftover temp file has to be before [`sweep_stale_temps`]
/// removes it. No write takes this long, so nothing in progress is touched.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Remove temp files that an [`atomic_write`] interrupted by a crash or
/// power loss left behind, in every folder blink writes to. Only files
/// named as `atomic_write` names them and older than [`STALE_TEMP_AGE`]
/// go, so another blink's write in progress is safe. Best effort.
pub fn sweep_stale_temps() {
    let dirs = [root_dir(), sessions_dir(), checkpoints_dir()];
    for dir in dirs.into_iter().flatten() {
        sweep_stale_temps_in(&dir, STALE_TEMP_AGE);
    }
}

fn sweep_stale_temps_in(dir: &Path, age: std::time::Duration) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in read.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_temp_name) {
            continue;
        }
        // Not following links: only plain files are ours.
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|d| d >= age);
        if meta.is_file() && old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Default local working directory: the user's home dir.
///
/// Without one, the current directory; without that either, the root of
/// the filesystem (on Windows, of the system drive), so the TUI always has
/// an absolute, navigable starting point.
pub fn default_local_dir() -> PathBuf {
    if let Some(home) = directories::UserDirs::new() {
        return home.home_dir().to_path_buf();
    }
    env::current_dir().unwrap_or_else(|_| fallback_root(env::var("SystemDrive").ok().as_deref()))
}

/// The filesystem root: `/`, or on Windows the system drive's (`C:\` if
/// `%SystemDrive%` isn't set).
fn fallback_root(system_drive: Option<&str>) -> PathBuf {
    if cfg!(windows) {
        let drive = system_drive.filter(|d| !d.is_empty()).unwrap_or("C:");
        PathBuf::from(format!("{drive}\\"))
    } else {
        let _ = system_drive;
        PathBuf::from("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- atomic_write -------------------------------------------------------

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_nothing_else() {
        let _home = test_home();
        let dir = root_dir().unwrap();
        let path = dir.join("f.ini");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, |f| {
            use std::io::Write as _;
            f.write_all(b"new")
        })
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(names_in(&dir), ["f.ini"]);
    }

    /// The temp file's name grows the target's, so a target name near the
    /// limit used to make every save fail with "file name too long".
    #[test]
    fn atomic_write_handles_a_name_at_the_length_limit() {
        let _home = test_home();
        let dir = root_dir().unwrap();
        let name = format!("{}é", "a".repeat(MAX_FILE_NAME_BYTES - 2));
        assert_eq!(name.len(), MAX_FILE_NAME_BYTES);
        let path = dir.join(&name);

        atomic_write(&path, |f| {
            use std::io::Write as _;
            f.write_all(b"new")
        })
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(names_in(&dir), [name]);
    }

    #[test]
    fn a_failed_atomic_write_keeps_the_file_and_cleans_up() {
        let _home = test_home();
        let dir = root_dir().unwrap();
        let path = dir.join("f.ini");
        fs::write(&path, "old").unwrap();

        let res = atomic_write(&path, |_| Err(std::io::Error::other("disk full")));

        assert!(res.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(names_in(&dir), ["f.ini"], "no temp file left behind");
    }

    /// Two blink processes saving one file — a theme cycle in each, say —
    /// shared one `<name>.tmp`: each truncated and wrote it under the other,
    /// so a mix of both could be renamed into place, and the slower rename
    /// failed outright. Writers here are threads, but the file system sees
    /// the same thing.
    #[test]
    fn concurrent_atomic_writes_each_land_whole() {
        let _home = test_home();
        let dir = root_dir().unwrap();
        let path = dir.join("f.ini");
        let contents: Vec<String> = (0..8)
            .map(|i| char::from(b'a' + i).to_string().repeat(64 * 1024))
            .collect();

        let errors: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = contents
                .iter()
                .map(|c| {
                    let path = path.clone();
                    s.spawn(move || {
                        (0..20)
                            .filter_map(|_| {
                                atomic_write(&path, |f| {
                                    use std::io::Write as _;
                                    // In pieces, as `Ini::write_to` writes.
                                    for chunk in c.as_bytes().chunks(4096) {
                                        f.write_all(chunk)?;
                                    }
                                    Ok(())
                                })
                                .err()
                                .map(|e| e.to_string())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });

        assert!(errors.is_empty(), "{errors:?}");
        let landed = fs::read_to_string(&path).unwrap();
        assert!(contents.contains(&landed), "a mix of writers landed");
        assert_eq!(names_in(&dir), ["f.ini"]);
    }

    /// The temp file is opened create-new: something already at its name —
    /// a link planted there — is never written through. Planted at the name
    /// actually used; an earlier version of this test planted at `f.tmp`,
    /// a name the temp file hadn't had in a while, and so tested nothing.
    #[cfg(unix)]
    #[test]
    fn atomic_write_never_writes_through_an_existing_file() {
        let _home = test_home();
        let dir = root_dir().unwrap();
        let path = dir.join("f.ini");
        let victim = dir.join("victim");
        fs::write(&victim, "untouched").unwrap();
        let tmp = temp_path(&path, 1, 2);
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();

        let err = atomic_write_via(
            &path,
            &tmp,
            |f| {
                use std::io::Write as _;
                f.write_all(b"new")
            },
            sync_parent_dir,
        )
        .expect_err("an occupied temp name is refused");

        assert!(err.to_string().contains("exists"), "{err}");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(!path.exists());
    }

    #[test]
    fn temp_names_are_recognised_exactly() {
        let tmp = temp_path(Path::new("/x/f.ini"), 42, 7);
        assert_eq!(tmp, Path::new("/x/.f.ini.42.7.tmp"));
        assert!(is_temp_name(".f.ini.42.7.tmp"));
        assert!(is_temp_name(".a.b.c.1.0.tmp"));
        for not in [
            ".f.ini.tmp",
            ".42.7.tmp",
            "f.ini.42.7.tmp",
            ".f.x.7.tmp",
            ".f.1..tmp",
            ".hidden.tmp",
        ] {
            assert!(!is_temp_name(not), "{not}");
        }
    }

    /// Created umask-default (0644), a file was private only while its
    /// folder was, and each save undid a `chmod 600`.
    #[cfg(unix)]
    #[test]
    fn atomic_write_makes_the_file_private_and_keeps_it_so() {
        use std::os::unix::fs::PermissionsExt;
        let _home = test_home();
        let path = root_dir().unwrap().join("f.ini");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write(&path, |f| {
            use std::io::Write as _;
            f.write_all(b"new")
        })
        .unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// Once the rename is done the file is saved; a folder that won't sync
    /// (some FUSE and network mounts) made every save report failure.
    #[test]
    fn a_folder_that_wont_sync_does_not_fail_a_finished_save() {
        let _home = test_home();
        let path = root_dir().unwrap().join("f.ini");

        atomic_write_via(
            &path,
            &temp_path(&path, 1, 1),
            |f| {
                use std::io::Write as _;
                f.write_all(b"new")
            },
            |_| Err(BlinkError::config("EINVAL")),
        )
        .expect("the file is in place, so the save succeeded");

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    // -- the app folders ----------------------------------------------------

    /// A folder made by hand or restored at 0755 stayed that way, and with
    /// it every file inside was readable by other users.
    #[cfg(unix)]
    #[test]
    fn an_existing_folder_open_to_others_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let _home = test_home();
        let sessions = root_dir().unwrap().join("sessions");
        fs::create_dir(&sessions).unwrap();
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o755)).unwrap();

        sessions_dir().unwrap();

        let mode = fs::metadata(&sessions).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let shown = sessions.display().to_string();
        assert!(
            take_notices()
                .iter()
                .any(|n| n.contains(&shown) && n.contains("made it private")),
            "the change is reported"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_private_folder_is_left_alone_and_not_reported() {
        let _home = test_home();
        sessions_dir().unwrap();
        let shown = root_dir().unwrap().join("sessions").display().to_string();
        sessions_dir().unwrap();
        assert!(!take_notices().iter().any(|n| n.contains(&shown)));
    }

    /// A file where a folder belongs is said plainly, not left to fail as
    /// "Not a directory" at whatever opens a file inside it.
    #[test]
    fn a_file_where_a_folder_belongs_is_an_error() {
        let _home = test_home();
        fs::write(root_dir().unwrap().join("themes"), "x").unwrap();

        let err = themes_dir().expect_err("not a folder");
        assert!(err.to_string().contains("is not a directory"), "{err}");
    }

    // -- leftover temp files ----------------------------------------------

    #[test]
    fn stale_temp_files_are_swept_and_nothing_else() {
        let _home = test_home();
        let dir = root_dir().unwrap();
        let old = dir.join(".f.ini.1.0.tmp");
        let fresh = dir.join(".f.ini.1.1.tmp");
        let other = dir.join(".notes.tmp");
        let real = dir.join("f.ini");
        for p in [&old, &fresh, &other, &real] {
            fs::write(p, "x").unwrap();
        }
        let long_ago = std::time::SystemTime::now() - 2 * STALE_TEMP_AGE;
        for p in [&old, &other, &real] {
            fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }

        sweep_stale_temps_in(&dir, STALE_TEMP_AGE);

        assert!(!old.exists(), "an old leftover goes");
        assert!(fresh.exists(), "one that may still be being written stays");
        assert!(other.exists(), "a file not named as ours stays");
        assert!(real.exists());
    }

    #[test]
    fn the_windows_fallback_root_is_the_system_drive() {
        if cfg!(windows) {
            assert_eq!(fallback_root(Some("D:")), PathBuf::from("D:\\"));
            assert_eq!(fallback_root(None), PathBuf::from("C:\\"));
        } else {
            assert_eq!(fallback_root(Some("D:")), PathBuf::from("/"));
        }
    }

    #[test]
    fn a_guard_redirects_away_from_the_shared_home() {
        let shared = base_dir().expect("shared home");
        assert_eq!(
            shared,
            std::env::temp_dir().join(format!("blink-test-{}", std::process::id())),
            "without a guard, tests share one per-process scratch home",
        );

        let _home = test_home();
        assert_ne!(base_dir().expect("private home"), shared);
    }

    #[test]
    fn the_override_is_cleared_when_the_guard_drops() {
        let shared = base_dir().expect("shared home");
        {
            let _home = test_home();
        }
        assert_eq!(
            base_dir().expect("shared home again"),
            shared,
            "a dropped guard must not leave the thread pointing at a deleted directory",
        );
    }

    #[test]
    fn guards_on_different_threads_get_different_homes() {
        use std::sync::mpsc;

        let _home = test_home();
        let mine = base_dir().expect("my home");

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let other = std::thread::spawn(move || {
            let _home = test_home();
            acquired_tx
                .send(base_dir().expect("their home"))
                .expect("report this thread's home");
            // Hold the guard open until the main thread has read its own.
            release_rx.recv().expect("wait for the main thread");
        });

        let theirs = acquired_rx
            .recv()
            .expect("the other thread must report its home");

        // The load-bearing read: another thread's guard is alive right now.
        // With a process-global override this returns `theirs`; only a
        // thread-local one still answers `mine`.
        assert_eq!(
            base_dir().expect("my home while theirs is live"),
            mine,
            "a live guard on another thread must not change this thread's home",
        );
        assert_ne!(mine, theirs, "each thread's guard gets its own directory");

        release_tx.send(()).expect("release the other thread");
        other.join().expect("the spawned thread must not panic");
    }

    #[test]
    fn a_nested_guard_does_not_demote_the_outer_guard_when_it_drops() {
        let outer = test_home();
        let outer_dir = outer.path().to_path_buf();
        {
            let inner = test_home();
            assert_ne!(inner.path(), outer_dir);
        }
        assert_eq!(
            base_dir().expect("outer home restored"),
            outer_dir,
            "dropping the inner guard must restore the outer guard's \
             directory, not demote it to the shared per-process directory",
        );
    }

    #[test]
    fn a_guard_removes_its_tree_even_when_a_test_panics() {
        // Drop-on-unwind is the whole reason this is a guard rather than a
        // cleanup call at the end of a test: a test that fails is exactly
        // when its directory would otherwise be left behind.
        let payload = std::panic::catch_unwind(|| {
            let home = test_home();
            let sessions = sessions_dir().expect("sessions dir");
            std::fs::write(sessions.join("t.ini"), b"x").expect("write a session file");
            std::panic::panic_any(home.path().to_path_buf());
        })
        .expect_err("the closure must have panicked");

        let dir = *payload
            .downcast::<PathBuf>()
            .expect("the panic payload carries the guard's directory");
        assert!(!dir.exists(), "unwinding must still run the guard's Drop");
    }

    // -- environment validation ------------------------------------------
    //
    // These cover `real_base_dir`'s rules without touching the process
    // environment: `base_dir_from` takes the values as arguments, so the
    // logic is reachable without `unsafe` env mutation (which would also
    // race every other test in this binary).

    #[test]
    fn an_absolute_env_value_is_accepted() {
        let p = require_absolute("/srv/config", "$HOME").expect("absolute is valid");
        assert_eq!(p, PathBuf::from("/srv/config"));
    }

    #[test]
    fn a_relative_env_value_is_rejected_and_names_the_variable() {
        let err = require_absolute("relative/path", "$HOME")
            .expect_err("a relative path cannot be a config home");
        assert!(err.to_string().contains("$HOME"), "{err}");
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_config_home_wins_when_it_is_absolute() {
        let dir = base_dir_from(Some("/xdg"), Some("/home/u")).expect("valid");
        assert_eq!(dir, PathBuf::from("/xdg/blink"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_relative_xdg_config_home_is_an_error_rather_than_a_fallback() {
        // Falling back to $HOME here would silently ignore a setting the
        // user got wrong, putting their config somewhere they did not ask
        // for. The error is the point.
        let err = base_dir_from(Some("relative"), Some("/home/u"))
            .expect_err("a relative XDG_CONFIG_HOME must not be tolerated");
        assert!(err.to_string().contains("XDG_CONFIG_HOME"), "{err}");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_empty_xdg_config_home_falls_through_to_home() {
        // Set-but-empty means "unset" here. It is invisible, load-bearing,
        // and one edit away from becoming a bare `is_some()` check.
        let dir = base_dir_from(Some(""), Some("/home/u")).expect("valid");
        assert_eq!(dir, PathBuf::from("/home/u/.config/blink"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn home_is_used_when_xdg_config_home_is_unset() {
        let dir = base_dir_from(None, Some("/home/u")).expect("valid");
        assert_eq!(dir, PathBuf::from("/home/u/.config/blink"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_unset_home_is_an_error() {
        let err = base_dir_from(None, None).expect_err("nothing left to resolve from");
        assert!(err.to_string().contains("$HOME is not set"), "{err}");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_relative_home_is_an_error() {
        let err = base_dir_from(None, Some("u")).expect_err("relative $HOME");
        assert!(err.to_string().contains("$HOME"), "{err}");
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_falls_back_to_the_application_support_directory() {
        let dir = base_dir_from(None, Some("/Users/u")).expect("valid");
        assert_eq!(
            dir,
            PathBuf::from("/Users/u/Library/Application Support/blink"),
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_puts_the_config_under_app_data() {
        let dir = base_dir_from(Some(r"C:\Users\u\AppData\Roaming")).expect("valid");
        assert_eq!(dir, PathBuf::from(r"C:\Users\u\AppData\Roaming\blink"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_unset_app_data_is_an_error() {
        let err = base_dir_from(None).expect_err("nothing to resolve from");
        assert!(err.to_string().contains("%APPDATA%"), "{err}");
    }

    /// Plants a symlink at `<base>/planted` pointing at a populated
    /// `<base>/victim`, standing in for an attacker who got there first.
    #[cfg(unix)]
    fn plant_symlink(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("blink-claim-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch base");

        let victim = base.join("victim");
        std::fs::create_dir(&victim).expect("victim dir");
        std::fs::write(victim.join("secret"), b"x").expect("victim file");

        let planted = base.join("planted");
        symlink(&victim, &planted).expect("plant the symlink");

        (base, victim, planted)
    }

    #[cfg(unix)]
    #[test]
    fn an_occupied_path_is_refused_rather_than_adopted() {
        // This is what closes the race: even if an attacker replants between
        // the sweep and the creation, `mkdir` refuses the path outright
        // instead of writing through whatever is there.
        let (base, victim, planted) = plant_symlink("refuse");

        let err = test_home::create_exclusive(&planted)
            .expect_err("an occupied path must be refused, never adopted");

        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            victim.join("secret").exists(),
            "refusing must not disturb whatever the link pointed at",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn claiming_a_path_removes_a_planted_symlink_without_following_it() {
        use std::os::unix::fs::PermissionsExt;

        let (base, victim, planted) = plant_symlink("claim");

        test_home::claim_dir(&planted).expect("the sweep must clear the way");

        let meta = std::fs::symlink_metadata(&planted).expect("claimed path");
        assert!(
            meta.file_type().is_dir(),
            "the claimed path is a real directory"
        );
        assert!(
            !meta.file_type().is_symlink(),
            "and no longer the planted link"
        );
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o700,
            "a config home must not be readable by other users",
        );
        assert!(
            victim.join("secret").exists(),
            "only the link is removed, never its target",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_written_under_a_guard_is_gone_when_the_guard_drops() {
        let dir;
        {
            let home = test_home();
            dir = home.path().to_path_buf();
            let sessions = sessions_dir().expect("sessions dir");
            std::fs::write(sessions.join("t.ini"), b"x").expect("write a session file");
            assert!(sessions.join("t.ini").exists());
        }
        assert!(!dir.exists(), "the guard must remove its whole tree");
    }
}
