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
/// [`test_home`]. That guarantee rests on `cfg(test)` being set for every
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
        OVERRIDE
            .with(|o| o.borrow().clone())
            .unwrap_or_else(shared)
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
                let dir = std::env::temp_dir()
                    .join(format!("blink-test-{}", std::process::id()));
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
        let dir = std::env::temp_dir()
            .join(format!("blink-test-{}-{n}", std::process::id()));
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

#[cfg(target_os = "linux")]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
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
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
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
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
    let user_profile = env::var("USERPROFILE")
        .map_err(|_| BlinkError::config("%USERPROFILE% is not set"))?;
    let profile_path = PathBuf::from(&user_profile);
    if !profile_path.is_absolute() {
        return Err(BlinkError::config("%USERPROFILE% must be an absolute path"));
    }
    Ok(profile_path.join("Documents").join(APP_DIR_NAME))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[cfg_attr(test, allow(dead_code))]
fn real_base_dir() -> Result<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

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

        let theirs = acquired_rx.recv().expect("the other thread must report its home");

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

    /// Plants a symlink at `<base>/planted` pointing at a populated
    /// `<base>/victim`, standing in for an attacker who got there first.
    #[cfg(unix)]
    fn plant_symlink(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir()
            .join(format!("blink-claim-{tag}-{}", std::process::id()));
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
        assert!(meta.file_type().is_dir(), "the claimed path is a real directory");
        assert!(!meta.file_type().is_symlink(), "and no longer the planted link");
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
