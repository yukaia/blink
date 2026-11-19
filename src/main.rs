//! blink — terminal SFTP/SCP/FTP/FTPS client.

use std::io::Write;
use std::process::ExitCode;

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
    /// Show interrupted batch transfers that can be resumed.
    ///
    /// A checkpoint is written before each batch transfer and updated as
    /// jobs complete. If blink is killed mid-batch, the next run can pick
    /// up where it left off: use `r` / `R` in the Transfers pane to resume
    /// a download / upload batch.
    ///
    /// Pass --clean to remove checkpoints that are no longer useful: files
    /// whose batch fully completed, or that belong to a session that no
    /// longer exists or now points at another server. Pass --force to
    /// remove every checkpoint file regardless of state, along with any
    /// temporary files an interrupted save left behind. Neither asks first.
    Checkpoints {
        /// Remove stale checkpoints (fully completed or orphaned).
        #[arg(long)]
        clean: bool,
        /// Remove every checkpoint file, whatever its state.
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

fn main() -> ExitCode {
    let log_warnings = init_tracing();
    let cli = Cli::parse();
    let result = run(cli, log_warnings);
    // Anything noticed that the TUI didn't get to show — or that came from
    // a command that has no TUI, or from one that failed before its TUI
    // started — goes to stderr.
    for n in paths::take_notices() {
        eprintln!("note: {}", sanitize_display(&n));
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", error_line(&e));
            ExitCode::FAILURE
        }
    }
}

/// How a failed command reports itself: the error's own message. Returning
/// the error from `main` printed its `Debug` form instead, as in
/// `Error: SessionNotFound("x")`.
fn error_line(e: &crate::error::BlinkError) -> String {
    format!("error: {}", sanitize_display(&e.to_string()))
}

/// Run one command. `log_warnings` are about logging itself (a bad
/// `BLINK_LOG`, an unusable `BLINK_LOG_FILE`): the TUI puts them in its log,
/// where they can be read, and anything else prints them first.
#[tokio::main]
async fn run(cli: Cli, log_warnings: Vec<String>) -> Result<()> {
    let Some(command) = cli.command else {
        let (config, theme, warnings) = tui_setup(log_warnings)?;
        return tui::run(config, theme, warnings).await;
    };
    let tui_command = matches!(command, Command::Open { .. } | Command::Connect { .. });
    if !tui_command {
        for w in &log_warnings {
            eprintln!("warning: {w}");
        }
    }
    match command {
        Command::Sessions => {
            list_sessions(&mut std::io::stdout().lock(), &mut std::io::stderr().lock())
        }
        Command::Themes => {
            list_themes(&mut std::io::stdout().lock(), &mut std::io::stderr().lock())
        }
        Command::Open { name } => {
            let session = find_session(&name, &mut std::io::stderr().lock())?;
            let (config, theme, warnings) = tui_setup(log_warnings)?;
            // Loaded from disk — already saved.
            tui::run_with_session(config, theme, warnings, session, false).await
        }
        Command::Connect { url } => {
            let session = session::Session::from_url(&url)?;
            let (config, theme, warnings) = tui_setup(log_warnings)?;
            // Built from a URL — nothing on disk behind it.
            tui::run_with_session(config, theme, warnings, session, true).await
        }
        Command::Checkpoints { clean, force } => list_checkpoints(clean, force),
        Command::KnownHosts { action } => match action {
            KnownHostsAction::Remove { host, port } => {
                remove_known_host(&mut std::io::stdout().lock(), &host, port)
            }
        },
    }
}

/// What the TUI needs before it starts, and the warnings to open its log
/// with. Only the TUI loads `config.ini`: no other command uses it, so a
/// broken one shouldn't stop `blink known-hosts remove` and the rest. A
/// broken one does stop the TUI, rather than run it on settings you didn't
/// choose.
fn tui_setup(mut warnings: Vec<String>) -> Result<(Config, Theme, Vec<String>)> {
    let (config, config_warnings) = Config::load()?;
    warnings.extend(config_warnings);
    let (theme, theme_warning) = Theme::load_or_fallback(&config.general.theme);
    warnings.extend(theme_warning);
    // Temp files a crash left behind. Long-dead ones only; see the fn.
    paths::sweep_stale_temps();
    // Made here, by the command that writes, so there's a folder to find
    // when adding a theme; listing themes only reads.
    let _ = paths::themes_dir();
    Ok((config, theme, warnings))
}

/// The saved session called `name`. When there's none, any session files
/// that couldn't be read are named on `err` first: the one wanted may be
/// among them, and "not found" alone would hide that.
fn find_session(name: &str, err: &mut impl Write) -> Result<session::Session> {
    let listing = session::Session::list_all_detailed()?;
    if let Some(s) = listing.sessions.into_iter().find(|s| s.name == name) {
        return Ok(s);
    }
    for skip in &listing.skipped {
        writeln!(err, "warning: skipped {}", sanitize_display(skip))?;
    }
    Err(crate::error::BlinkError::session_not_found(name))
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

/// Set up logging, returning any warnings about it for the caller to show:
/// printed from here they'd be hidden as soon as the TUI took the screen.
fn init_tracing() -> Vec<String> {
    use tracing_subscriber::fmt;

    let (filter, warning) = log_filter(std::env::var("BLINK_LOG").ok().as_deref());
    let mut warnings: Vec<String> = warning.into_iter().collect();

    // If BLINK_LOG_FILE is set, write logs there; otherwise discard them so
    // they don't smear the TUI. Example: BLINK_LOG_FILE=/tmp/blink.log BLINK_LOG=debug blink
    if let Ok(log_path) = std::env::var("BLINK_LOG_FILE") {
        match open_log_file(&log_path) {
            Ok((file, warning)) => {
                warnings.extend(warning);
                // One handle shared by every line: `&File` writes, so there
                // is no need to duplicate the descriptor per event.
                let file = std::sync::Arc::new(file);
                let _ = fmt()
                    .with_env_filter(filter)
                    // Server-supplied text reaches the log through tracing
                    // fields; see `SanitizedLog`.
                    .with_writer(move || crate::error::SanitizedLog(SharedFile(file.clone())))
                    .with_ansi(false)
                    .try_init();
                return warnings;
            }
            Err(e) => warnings.push(format!(
                "could not open BLINK_LOG_FILE={}, logs discarded: {e}",
                sanitize_display(&log_path),
            )),
        }
    }

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::sink)
        .try_init();
    warnings
}

/// The log file, shared between log lines.
struct SharedFile(std::sync::Arc<std::fs::File>);

impl Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

/// The log filter for a `BLINK_LOG` value, and a warning if it wasn't
/// usable. An unparseable value used to fall back to `warn` silently, so
/// `BLINK_LOG=debgu` looked like debug logging that logged nothing.
fn log_filter(value: Option<&str>) -> (tracing_subscriber::EnvFilter, Option<String>) {
    use tracing_subscriber::EnvFilter;
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return (EnvFilter::new("warn"), None);
    };
    match EnvFilter::try_new(value) {
        Ok(f) => (f, None),
        Err(e) => (
            EnvFilter::new("warn"),
            Some(format!(
                "BLINK_LOG={} isn't a valid filter ({e}); logging at warn",
                sanitize_display(value)
            )),
        ),
    }
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
                    "{} is readable by other users (mode {:o}); \
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

/// One saved session per line: name, protocol, `user@host:port`.
///
/// The name column is as wide as the widest name, measured in terminal
/// columns, so long and wide-character names don't push the rest out of
/// line. An IPv6 host is bracketed, as in a URL: `u@::1:22` can't be told
/// apart from a host `::1:22` with no port.
fn list_sessions(out: &mut impl Write, err: &mut impl Write) -> Result<()> {
    use ratatui::text::Span;

    let listing = session::Session::list_all_detailed()?;
    let names: Vec<String> = listing
        .sessions
        .iter()
        .map(|s| sanitize_display(&s.name).into_owned())
        .collect();
    let width = names
        .iter()
        .map(|n| Span::raw(n.as_str()).width())
        .max()
        .unwrap_or(0)
        .max(14);
    for (s, name) in listing.sessions.iter().zip(&names) {
        let pad = " ".repeat(width - Span::raw(name.as_str()).width());
        let host = sanitize_display(&s.host);
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.into_owned()
        };
        writeln!(
            out,
            "{name}{pad}  {:<6}  {}@{host}:{}",
            s.protocol.as_str(),
            sanitize_display(&s.username),
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
/// (like `blink sessions` does) rather than listed. So is a configured
/// theme that won't load, since the TUI would fall back from it.
fn list_themes(out: &mut impl Write, err: &mut impl Write) -> Result<()> {
    let listing = Theme::list_detailed();
    let builtins = Theme::list_builtin_names();
    for entry in &listing.entries {
        // Names are already refused if they hold control characters; this
        // is the backstop for anything else that shouldn't reach a terminal.
        let key = sanitize_display(&entry.key);
        if !entry.user {
            writeln!(out, "{key}")?;
            continue;
        }
        match Theme::load(&entry.key) {
            Ok(_) if builtins.contains(&entry.key.as_str()) => {
                writeln!(out, "{key} (user, replaces the built-in)")?
            }
            Ok(_) => writeln!(out, "{key} (user)")?,
            Err(e) => writeln!(
                err,
                "warning: skipped theme {key}: {}",
                sanitize_display(&e.to_string())
            )?,
        }
    }
    // Read without creating anything: a missing config.ini is the default,
    // and a broken one is the TUI's to refuse, not this listing's.
    if let Ok(path) = paths::lookup("config.ini")
        && let Ok(config) = Config::load_from(&path)
        && let (_, Some(why)) = Theme::load_or_fallback(&config.general.theme)
    {
        writeln!(err, "warning: configured {}", sanitize_display(&why))?;
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

    /// `u@::1:2222` can't be told from a host `::1:2222` with no port.
    #[test]
    fn sessions_bracket_an_ipv6_host() {
        let _home = crate::paths::test_home();
        let mut s = session::Session::from_url("sftp://alice@[::1]:2222").unwrap();
        s.name = "v6".into();
        s.save().unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_sessions(&mut out, &mut err).unwrap();
        assert!(text(out).ends_with("alice@[::1]:2222\n"));
    }

    /// Padded to 14 chars, a longer or wide-character name pushed its row
    /// out of line with the rest.
    #[test]
    fn sessions_line_up_whatever_the_names() {
        let _home = crate::paths::test_home();
        for name in ["a", "a-much-longer-session-name", "日本語のセッション"] {
            let mut s = session::Session::from_url("sftp://u@h.example").unwrap();
            s.name = name.into();
            s.save().unwrap();
        }

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_sessions(&mut out, &mut err).unwrap();
        let out = text(out);
        let columns: Vec<usize> = out
            .lines()
            .map(|l| {
                let at = l.find("sftp").unwrap();
                ratatui::text::Span::raw(&l[..at]).width()
            })
            .collect();
        assert_eq!(columns.len(), 3);
        assert!(columns.windows(2).all(|w| w[0] == w[1]), "{out}");
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

    // --- running a command ---

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("blink").chain(args.iter().copied())).unwrap()
    }

    fn break_config() {
        std::fs::write(
            crate::paths::config_file().unwrap(),
            "[general]\nparallel_downloads = x\n",
        )
        .unwrap();
    }

    /// Returned from `main`, an error printed its `Debug` form:
    /// `Error: SessionNotFound("x")`.
    #[test]
    fn a_failed_command_reports_its_message() {
        let line = error_line(&crate::error::BlinkError::session_not_found("x"));
        assert_eq!(line, "error: session not found: x");
    }

    /// Only the TUI uses config.ini; a broken one stopped every command,
    /// including the ones you'd use to sort things out.
    #[test]
    fn a_broken_config_does_not_stop_commands_that_dont_use_it() {
        let _home = crate::paths::test_home();
        break_config();
        for args in [
            &["sessions"][..],
            &["themes"],
            &["checkpoints"],
            &["known-hosts", "remove", "h.example"],
        ] {
            run(cli(args), vec![]).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    /// The TUI still refuses to start on settings you didn't choose.
    #[test]
    fn a_broken_config_still_stops_the_tui() {
        let _home = crate::paths::test_home();
        break_config();
        for args in [&[][..], &["connect", "sftp://u@h.example"]] {
            let e = run(cli(args), vec![]).expect_err("refused").to_string();
            assert!(e.contains("parallel_downloads"), "{args:?}: {e}");
        }
    }

    /// What `config.ini` had ignored or corrected went to the debug log
    /// only, so a mistyped setting just didn't work.
    #[test]
    fn config_warnings_open_the_tuis_log() {
        let _home = crate::paths::test_home();
        std::fs::write(
            crate::paths::config_file().unwrap(),
            "[general]\nconfim_quit = false\n",
        )
        .unwrap();
        let (_, _, warnings) = tui_setup(vec!["from logging".into()]).unwrap();
        assert_eq!(warnings[0], "from logging");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unknown setting `confim_quit`")),
            "{warnings:?}"
        );
    }

    /// Listing only reads: on a fresh machine it left a config.ini and
    /// every folder behind.
    #[test]
    fn listing_commands_create_nothing() {
        let home = crate::paths::test_home();
        for args in [&["sessions"][..], &["themes"], &["checkpoints"]] {
            run(cli(args), vec![]).unwrap();
        }
        let left: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// `open` skipped unreadable files silently, so a broken `work.ini`
    /// was just "session not found".
    #[test]
    fn open_names_unreadable_session_files_when_nothing_matches() {
        let _home = crate::paths::test_home();
        std::fs::write(
            crate::paths::sessions_dir().unwrap().join("broken.ini"),
            "[session]\nname = work\nport = x\n",
        )
        .unwrap();

        let mut err = Vec::new();
        let e = find_session("work", &mut err).expect_err("not found");
        assert!(e.to_string().contains("session not found"), "{e}");
        let err = text(err);
        assert!(err.starts_with("warning: skipped broken.ini"), "{err}");
    }

    #[test]
    fn open_finds_a_session_and_says_nothing_else() {
        let _home = crate::paths::test_home();
        let mut s = session::Session::from_url("sftp://alice@files.example").unwrap();
        s.name = "work".into();
        s.save().unwrap();
        std::fs::write(
            crate::paths::sessions_dir().unwrap().join("broken.ini"),
            "nope",
        )
        .unwrap();

        let mut err = Vec::new();
        assert_eq!(find_session("work", &mut err).unwrap().name, "work");
        assert!(err.is_empty());
    }

    // --- logging ---

    /// An unparseable `BLINK_LOG` fell back to `warn` without a word.
    #[test]
    fn a_bad_blink_log_value_is_warned_about() {
        let (_, w) = log_filter(Some("nonsense=[["));
        let w = w.expect("warned");
        assert!(w.contains("BLINK_LOG=nonsense=[["), "{w}");
        assert!(w.contains("logging at warn"), "{w}");
        for ok in [None, Some(""), Some("debug"), Some("blink=trace,warn")] {
            assert!(log_filter(ok).1.is_none(), "{ok:?}");
        }
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

    /// A theme file named with an escape sequence put it raw on the
    /// terminal. Such a name can't load, so it's skipped, and shown safely.
    #[test]
    fn themes_never_print_a_raw_escape() {
        let _home = crate::paths::test_home();
        let dir = crate::paths::themes_dir().unwrap();
        std::fs::write(dir.join("a\x1b[31mred.ini"), "[colors]\n").unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_themes(&mut out, &mut err).unwrap();
        let (out, err) = (text(out), text(err));
        assert!(
            !out.contains('\x1b') && !err.contains('\x1b'),
            "{out:?} {err:?}"
        );
        assert!(!out.contains("red"), "{out}");
        assert!(err.contains("warning: skipped a"), "{err}");
    }

    /// The TUI falls back from a configured theme that won't load; listing
    /// themes is where you'd look, so it says so.
    #[test]
    fn themes_warn_when_the_configured_theme_wont_load() {
        let _home = crate::paths::test_home();
        std::fs::write(
            crate::paths::config_file().unwrap(),
            "[general]\ntheme = long-gone\n",
        )
        .unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list_themes(&mut out, &mut err).unwrap();
        let err = text(err);
        assert!(
            err.contains("warning: configured theme `long-gone` not found; using dracula"),
            "{err}"
        );
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
