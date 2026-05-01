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
use crate::error::Result;
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

/// Strip control characters from user-supplied strings before printing them to
/// the terminal. This prevents ANSI escape sequences embedded in session
/// names, usernames, or hostnames from injecting terminal commands.
fn sanitize_display(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().all(|c| !c.is_control()) {
        std::borrow::Cow::Borrowed(s)
    } else {
        std::borrow::Cow::Owned(s.chars().filter(|c| !c.is_control()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_display_clean_borrows() {
        let out = sanitize_display("hello");
        assert_eq!(&*out, "hello");
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn sanitize_display_strips_control_chars() {
        let out = sanitize_display("evil\x1b[31mred\x07");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert!(out.contains("evil"));
        assert!(out.contains("red"));
    }

    #[test]
    fn sanitize_display_null_byte_stripped() {
        let out = sanitize_display("host\x00name");
        assert!(!out.contains('\x00'));
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_env("BLINK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // Send logs to a sink while the TUI is up so they don't smear the screen.
    // A future refinement: write to a file under paths::root_dir()/blink.log.
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
