//! blink — terminal SFTP/SCP/FTP/FTPS client.

use std::io::Write;

use clap::{Parser, Subcommand};

mod checkpoint;
mod config;
mod error;
mod highlight;
mod known_hosts;
mod paths;
mod preview;
mod session;
mod theme;
mod transfer;
mod transport;
mod tui;

use crate::config::Config;
use crate::error::{Result, sanitize_display};
use crate::theme::Theme;

#[derive(Parser, Debug)]
#[command(
    name = "blink",
    version,
    about = "Terminal SFTP/SCP/FTP/FTPS client",
    long_about = None,
    disable_version_flag = true,
)]
struct Cli {
    /// Print version and exit. Bound to both `-v` and `--version`.
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
    version: (),

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open a saved session by name.
    Open { name: String },
    /// Connect ad-hoc using a URL like `sftp://user@host:22`.
    Connect { url: String },
    /// List saved sessions.
    Sessions,
    /// List themes: the built-ins and your own.
    Themes,
    /// Show any interrupted walk checkpoints that can be resumed.
    ///
    /// A checkpoint is written before each batch transfer and updated as
    /// jobs complete.  If blink is killed mid-batch, the next run can
    /// pick up where it left off.  Use `r` / `R` in the Transfers pane
    /// to resume a download / upload batch interactively.
    ///
    /// Pass --clean to remove checkpoints that are no longer useful:
    /// files whose batch fully completed, or that belong to a session
    /// that no longer exists or now points at another server.  Pass
    /// --force to remove every checkpoint
    /// file regardless of state.
    Checkpoints {
        /// Remove stale checkpoints (fully completed or orphaned).
        #[arg(long)]
        clean: bool,
        /// Remove ALL checkpoint files without prompting.
        #[arg(long)]
        force: bool,
    },
    /// Inspect or edit the stored SSH host keys.
    KnownHosts {
        #[command(subcommand)]
        action: KnownHostsAction,
    },
}

#[derive(Subcommand, Debug)]
enum KnownHostsAction {
    /// Forget the stored key for a host, so the next connect asks you to
    /// verify the new one.
    ///
    /// Run this only when you know the server's key was legitimately
    /// replaced (a rebuild, a migration). If blink reported a mismatch you
    /// were not expecting, confirm the new fingerprint out of band first —
    /// a mismatch is also what a man-in-the-middle looks like.
    Remove {
        /// Hostname exactly as you connect to it.
        host: String,
        /// Port the entry was stored under.
        #[arg(long, default_value_t = 22)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = Config::load()?;
    // A theme that won't load is reported in the log once the TUI is up;
    // printed here it would be hidden behind the alternate screen.
    let (theme, theme_warning) = Theme::load_or_fallback(&config.general.theme);

    // Temp files a crash left behind. Long-dead ones only; see the fn.
    paths::sweep_stale_temps();

    let tui = matches!(
        cli.command,
        None | Some(Command::Open { .. }) | Some(Command::Connect { .. })
    );
    let result = match cli.command {
        None => tui::run(config, theme, theme_warning).await,
        Some(Command::Sessions) => {
            list_sessions(&mut std::io::stdout().lock(), &mut std::io::stderr().lock())
        }
        Some(Command::Themes) => {
            list_themes(&mut std::io::stdout().lock(), &mut std::io::stderr().lock())
        }
        Some(Command::Open { name }) => {
            let session = session::Session::list_all()?
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| crate::error::BlinkError::session_not_found(name.clone()))?;
            // Loaded from disk — already saved.
            tui::run_with_session(config, theme, theme_warning, session, false).await
        }
        Some(Command::Connect { url }) => {
            let session = session::Session::from_url(&url)?;
            // Built from a URL — nothing on disk behind it.
            tui::run_with_session(config, theme, theme_warning, session, true).await
        }
        Some(Command::Checkpoints { clean, force }) => list_checkpoints(clean, force),
        Some(Command::KnownHosts { action }) => match action {
            KnownHostsAction::Remove { host, port } => {
                remove_known_host(&mut std::io::stdout().lock(), &host, port)
            }
        },
    };
    // The TUI puts these in its log; anything else says them on stderr.
    if !tui {
        for n in paths::take_notices() {
            eprintln!("note: {n}");
        }
    }
    result
}

/// Forget the stored host key(s) for `host` on `port`.
fn remove_known_host(out: &mut impl Write, host: &str, port: u16) -> Result<()> {
    let removed = known_hosts::remove_host(host, port)?;
    let shown = sanitize_display(host);
    if removed == 0 {
        // Almost always a host-form mismatch rather than a real absence, so
        // say what was searched for instead of just "not found".
        writeln!(
            out,
            "no stored key for {shown} port {port} — nothing removed"
        )?;
        writeln!(
            out,
            "entries are stored per host AND port; check `{}` if the host looks right",
            known_hosts::known_hosts_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the known_hosts file".to_string()),
        )?;
        return Ok(());
    }
    let plural = if removed == 1 { "entry" } else { "entries" };
    writeln!(out, "removed {removed} {plural} for {shown} port {port}")?;
    writeln!(
        out,
        "the next connect will ask you to verify the server's key"
    )?;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("BLINK_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));

    // If BLINK_LOG_FILE is set, write logs there; otherwise discard them so
    // they don't smear the TUI. Example: BLINK_LOG_FILE=/tmp/blink.log BLINK_LOG=debug blink
    if let Ok(log_path) = std::env::var("BLINK_LOG_FILE") {
        if let Ok((file, warning)) = open_log_file(&log_path) {
            if let Some(warning) = warning {
                eprintln!("{warning}");
            }

            let _ = fmt()
                .with_env_filter(filter)
                // Server-supplied text reaches the log through tracing
                // fields; see `SanitizedLog`.
                .with_writer(move || match file.try_clone() {
                    Ok(f) => Box::new(crate::error::SanitizedLog(f)) as Box<dyn std::io::Write>,
                    // Out of file descriptors. Drop the line rather than
                    // panicking from inside a logging call.
                    Err(_) => Box::new(std::io::sink()),
                })
                .with_ansi(false)
                .try_init();
            return;
        }
        eprintln!(
            "warning: could not open BLINK_LOG_FILE={}, logs discarded",
            sanitize_display(&log_path),
        );
    }

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::sink)
        .try_init();
}

/// Open (creating if needed) the file named by `BLINK_LOG_FILE`, plus a
/// warning to print when other users can read it.
fn open_log_file(log_path: &str) -> std::io::Result<(std::fs::File, Option<String>)> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);

    // At debug level this file carries hostnames, usernames, and remote
    // paths. Unlike everything else blink writes, it lives at a user-chosen
    // path rather than inside the 0700 config dir, so it needs its own
    // restrictive mode instead of inheriting the umask default (typically
    // 0644).
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }

    let file = opts.open(log_path)?;

    // `mode` only applies when the file is created. A log file left over
    // from an older blink (or created by another tool) keeps its original
    // permissions, so say so rather than silently writing secrets into a
    // world-readable file.
    #[cfg(unix)]
    let warning = {
        use std::os::unix::fs::MetadataExt as _;
        file.metadata()
            .ok()
            .filter(|meta| meta.mode() & 0o077 != 0)
            .map(|meta| {
                format!(
                    "warning: {} is readable by other users (mode {:o}); \
                     logs may contain hostnames and remote paths",
                    sanitize_display(log_path),
                    meta.mode() & 0o7777,
                )
            })
    };
    #[cfg(not(unix))]
    let warning = None;
    Ok((file, warning))
}

fn list_sessions(out: &mut impl Write, err: &mut impl Write) -> Result<()> {
    let listing = session::Session::list_all_detailed()?;
    for s in &listing.sessions {
        writeln!(
            out,
            "{:<14}  {:<6}  {}@{}:{}",
            sanitize_display(&s.name),
            s.protocol.as_str(),
            sanitize_display(&s.username),
            sanitize_display(&s.host),
            s.port,
        )?;
    }
    // A session file that won't load is otherwise invisible: it just doesn't
    // appear above. Say so, on stderr so the listing stays pipeable.
    for skip in &listing.skipped {
        writeln!(err, "warning: skipped {}", sanitize_display(skip))?;
    }
    Ok(())
}

/// Every theme `t` cycles through, one key per line, user themes marked.
/// User files are loaded here, so one that would fail is reported on stderr
/// (like `blink sessions` does) rather than listed.
fn list_themes(out: &mut impl Write, err: &mut impl Write) -> Result<()> {
    let listing = Theme::list_detailed();
    let builtins = Theme::list_builtin_names();
    for entry in &listing.entries {
        if !entry.user {
            writeln!(out, "{}", entry.key)?;
            continue;
        }
        match Theme::load(&entry.key) {
            Ok(_) if builtins.contains(&entry.key.as_str()) => {
                writeln!(out, "{} (user, replaces the built-in)", entry.key)?
            }
            Ok(_) => writeln!(out, "{} (user)", entry.key)?,
            Err(e) => writeln!(
                err,
                "warning: skipped theme {}: {}",
                entry.key,
                sanitize_display(&e.to_string())
            )?,
        }
    }
    for name in &listing.invalid {
        writeln!(
            err,
            "warning: skipped {name}: the name can't be used as a theme name"
        )?;
    }
    Ok(())
}

fn list_checkpoints(clean: bool, force: bool) -> Result<()> {
    crate::checkpoint::list_and_clean(clean, force)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    // --- argument parsing ---

    #[test]
    fn both_version_flags_print_the_version() {
        for flag in ["-v", "--version"] {
            let err = Cli::try_parse_from(["blink", flag]).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion, "{flag}");
        }
    }

    #[test]
    fn no_subcommand_opens_the_tui() {
        assert!(Cli::try_parse_from(["blink"]).unwrap().command.is_none());
    }

    #[test]
    fn known_hosts_remove_defaults_to_port_22() {
        let cli = Cli::try_parse_from(["blink", "known-hosts", "remove", "h.example"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::KnownHosts {
                action: KnownHostsAction::Remove { ref host, port: 22 }
            }) if host == "h.example"
        ));

        let cli =
            Cli::try_parse_from(["blink", "known-hosts", "remove", "h", "--port", "2222"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::KnownHosts {
                action: KnownHostsAction::Remove { port: 2222, .. }
            })
        ));
    }

    #[test]
    fn checkpoints_flags_parse() {
        let cli = Cli::try_parse_from(["blink", "checkpoints", "--clean", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Checkpoints {
                clean: true,
                force: true
            })
        ));
    }

    // --- `blink sessions` ---

    #[test]
    fn sessions_lists_each_saved_session_on_one_line() {
        let _home = crate::paths::test_home();
        let mut s = session::Session::from_url("sftp://alice@files.example:2222").unwrap();
        s.name = "work".into();
        s.save().unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_sessions(&mut out, &mut err).unwrap();

        assert_eq!(
            text(out),
            "work            sftp    alice@files.example:2222\n"
        );
        assert!(err.is_empty());
    }

    #[test]
    fn sessions_with_none_saved_prints_nothing() {
        let _home = crate::paths::test_home();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_sessions(&mut out, &mut err).unwrap();
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn sessions_warns_on_stderr_about_a_file_that_will_not_load() {
        let _home = crate::paths::test_home();
        let dir = crate::paths::sessions_dir().unwrap();
        std::fs::write(dir.join("broken.ini"), "not an ini [[[").unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_sessions(&mut out, &mut err).unwrap();

        assert!(out.is_empty(), "the listing stays pipeable");
        let err = text(err);
        assert!(err.starts_with("warning: skipped broken.ini"), "{err}");
    }

    // --- `blink themes` ---

    #[test]
    fn themes_prints_every_builtin_one_per_line() {
        let _home = crate::paths::test_home();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_themes(&mut out, &mut err).unwrap();
        let out = text(out);
        let mut names: Vec<&str> = out.lines().collect();
        names.sort();
        let mut builtins = Theme::list_builtin_names();
        builtins.sort();
        assert_eq!(names, builtins);
        assert!(err.is_empty());
    }

    /// `t` cycles through user themes too, so `blink themes` lists them,
    /// marked; a file that won't load is a warning, as with sessions.
    #[test]
    fn themes_lists_user_themes_and_warns_about_broken_ones() {
        let _home = crate::paths::test_home();
        let dir = crate::paths::themes_dir().unwrap();
        std::fs::write(dir.join("mine.ini"), "[colors]\nbg=#000000\n").unwrap();
        std::fs::write(dir.join("nord.ini"), "[colors]\nbg=#000000\n").unwrap();
        std::fs::write(dir.join("broken.ini"), "[colors]\nbg=zzz\n").unwrap();
        std::fs::write(dir.join("x..y.ini"), "[colors]\n").unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_themes(&mut out, &mut err).unwrap();
        let (out, err) = (text(out), text(err));

        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"dracula"), "{out}");
        assert!(lines.contains(&"mine (user)"), "{out}");
        assert!(
            lines.contains(&"nord (user, replaces the built-in)"),
            "{out}"
        );
        assert!(!out.contains("broken"), "{out}");
        assert!(err.contains("warning: skipped theme broken: "), "{err}");
        assert!(err.contains("bad color for `bg`"), "{err}");
        assert!(err.contains("warning: skipped x..y.ini"), "{err}");
    }

    // --- `blink known-hosts remove` ---

    #[test]
    fn removing_a_stored_host_says_how_many_entries_went() {
        let _home = crate::paths::test_home();
        let path = known_hosts::known_hosts_path().unwrap();
        std::fs::write(
            &path,
            "a.example ssh-ed25519 AAAA\na.example ssh-rsa BBBB\nb.example ssh-ed25519 CCCC\n",
        )
        .unwrap();

        let mut out = Vec::new();
        remove_known_host(&mut out, "a.example", 22).unwrap();
        assert_eq!(
            text(out),
            "removed 2 entries for a.example port 22\n\
             the next connect will ask you to verify the server's key\n",
        );

        let mut out = Vec::new();
        remove_known_host(&mut out, "b.example", 22).unwrap();
        assert!(text(out).starts_with("removed 1 entry for b.example port 22\n"));
    }

    #[test]
    fn removing_an_unknown_host_names_what_was_searched_and_the_file() {
        let _home = crate::paths::test_home();
        let path = known_hosts::known_hosts_path().unwrap();
        std::fs::write(&path, "a.example ssh-ed25519 AAAA\n").unwrap();

        let mut out = Vec::new();
        remove_known_host(&mut out, "a.example", 2222).unwrap();
        let out = text(out);
        assert!(
            out.starts_with("no stored key for a.example port 2222 — nothing removed\n"),
            "{out}"
        );
        assert!(out.contains(&path.display().to_string()), "{out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a.example ssh-ed25519 AAAA\n"
        );
    }

    #[test]
    fn the_removed_host_is_sanitized_before_printing() {
        let _home = crate::paths::test_home();
        let mut out = Vec::new();
        remove_known_host(&mut out, "evil\x1b[2Jhost", 22).unwrap();
        assert!(!text(out).contains('\x1b'));
    }

    // --- BLINK_LOG_FILE ---

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(path).unwrap().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn a_new_log_file_is_private_to_the_user() {
        let home = crate::paths::test_home();
        let path = home.path().join("blink.log");

        let (_file, warning) = open_log_file(path.to_str().unwrap()).unwrap();

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(warning, None);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_world_readable_log_file_is_warned_about_and_left_as_is() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = crate::paths::test_home();
        let path = home.path().join("blink.log");
        std::fs::write(&path, "old line\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let (mut file, warning) = open_log_file(path.to_str().unwrap()).unwrap();

        let warning = warning.expect("a 0644 log file is warned about");
        assert!(
            warning.contains("readable by other users (mode 644)"),
            "{warning}"
        );
        assert_eq!(
            mode_of(&path),
            0o644,
            "blink does not chmod the user's file"
        );

        // Appends rather than truncating.
        file.write_all(b"new line\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "old line\nnew line\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_private_log_file_is_not_warned_about() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = crate::paths::test_home();
        let path = home.path().join("blink.log");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let (_file, warning) = open_log_file(path.to_str().unwrap()).unwrap();
        assert_eq!(warning, None);
    }

    #[test]
    fn a_log_file_in_a_missing_directory_is_an_error() {
        let home = crate::paths::test_home();
        let path = home.path().join("no-such-dir").join("blink.log");
        assert!(open_log_file(path.to_str().unwrap()).is_err());
    }
}
