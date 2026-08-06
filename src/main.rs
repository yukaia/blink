//! blink — terminal SFTP/SCP/FTP/FTPS client.

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
use crate::error::{sanitize_display, Result};
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
    /// List built-in themes.
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
    /// that no longer exists.  Pass --force to remove every checkpoint
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
    let theme = Theme::load(&config.general.theme).unwrap_or_else(|_| {
        eprintln!(
            "warning: theme `{}` not found, falling back to dracula",
            sanitize_display(&config.general.theme)
        );
        Theme::load("dracula").expect("dracula is always available")
    });

    match cli.command {
        None => tui::run(config, theme).await,
        Some(Command::Sessions) => list_sessions(),
        Some(Command::Themes) => list_themes(),
        Some(Command::Open { name }) => {
            let session = session::Session::list_all()?
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| {
                    crate::error::BlinkError::session_not_found(name.clone())
                })?;
            tui::run_with_session(config, theme, session).await
        }
        Some(Command::Connect { url }) => {
            let session = session::Session::from_url(&url)?;
            tui::run_with_session(config, theme, session).await
        }
        Some(Command::Checkpoints { clean, force }) => list_checkpoints(clean, force),
        Some(Command::KnownHosts { action }) => match action {
            KnownHostsAction::Remove { host, port } => remove_known_host(&host, port),
        },
    }
}

/// Forget the stored host key(s) for `host` on `port`.
fn remove_known_host(host: &str, port: u16) -> Result<()> {
    let removed = known_hosts::remove_host(host, port)?;
    let shown = sanitize_display(host);
    if removed == 0 {
        // Almost always a host-form mismatch rather than a real absence, so
        // say what was searched for instead of just "not found".
        println!("no stored key for {shown} port {port} — nothing removed");
        println!(
            "entries are stored per host AND port; check `{}` if the host looks right",
            known_hosts::known_hosts_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the known_hosts file".to_string()),
        );
        return Ok(());
    }
    let plural = if removed == 1 { "entry" } else { "entries" };
    println!("removed {removed} {plural} for {shown} port {port}");
    println!("the next connect will ask you to verify the server's key");
    Ok(())
}

fn init_tracing() {
    use std::fs::OpenOptions;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter =
        EnvFilter::try_from_env("BLINK_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));

    // If BLINK_LOG_FILE is set, write logs there; otherwise discard them so
    // they don't smear the TUI. Example: BLINK_LOG_FILE=/tmp/blink.log BLINK_LOG=debug blink
    if let Ok(log_path) = std::env::var("BLINK_LOG_FILE") {
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);

        // At debug level this file carries hostnames, usernames, and remote
        // paths. Unlike everything else blink writes, it lives at a
        // user-chosen path rather than inside the 0700 config dir, so it
        // needs its own restrictive mode instead of inheriting the umask
        // default (typically 0644).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }

        if let Ok(file) = opts.open(&log_path) {
            // `mode` only applies when the file is created. A log file left
            // over from an older blink (or created by another tool) keeps
            // its original permissions, so say so rather than silently
            // writing secrets into a world-readable file.
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                if let Ok(meta) = file.metadata()
                    && meta.mode() & 0o077 != 0
                {
                    eprintln!(
                        "warning: {} is readable by other users (mode {:o}); \
                         logs may contain hostnames and remote paths",
                        sanitize_display(&log_path),
                        meta.mode() & 0o7777,
                    );
                }
            }

            let _ = fmt()
                .with_env_filter(filter)
                .with_writer(move || match file.try_clone() {
                    Ok(f) => Box::new(f) as Box<dyn std::io::Write>,
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

fn list_sessions() -> Result<()> {
    let listing = session::Session::list_all_detailed()?;
    for s in &listing.sessions {
        println!(
            "{:<14}  {:<6}  {}@{}:{}",
            sanitize_display(&s.name),
            s.protocol.as_str(),
            sanitize_display(&s.username),
            sanitize_display(&s.host),
            s.port,
        );
    }
    // A session file that won't load is otherwise invisible: it just doesn't
    // appear above. Say so, on stderr so the listing stays pipeable.
    for skip in &listing.skipped {
        eprintln!("warning: skipped {}", sanitize_display(skip));
    }
    Ok(())
}

fn list_themes() -> Result<()> {
    for name in Theme::list_builtin_names() {
        println!("{name}");
    }
    Ok(())
}

fn list_checkpoints(clean: bool, force: bool) -> Result<()> {
    crate::checkpoint::list_and_clean(clean, force)
}
