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
    }
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
    for s in session::Session::list_all()? {
        println!(
            "{:<14}  {:<6}  {}@{}:{}",
            sanitize_display(&s.name),
            s.protocol.as_str(),
            sanitize_display(&s.username),
            sanitize_display(&s.host),
            s.port,
        );
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
