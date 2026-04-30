//! blink — terminal SFTP/SCP/FTP/FTPS client.

use clap::{Parser, Subcommand};

mod checkpoint;
mod config;
mod error;
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
    use crate::checkpoint::Checkpoint;

    let dir = crate::paths::checkpoints_dir()?;

    // Collect every .json file in the checkpoints directory. This catches
    // orphaned files from renamed / deleted sessions and ad-hoc connects,
    // which would be invisible if we only iterated saved sessions.
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();

    if entries.is_empty() {
        println!("no checkpoints found");
        return Ok(());
    }

    // Build a set of known session names so we can flag orphans.
    let known_sessions: std::collections::HashSet<String> = session::Session::list_all()
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.name)
        .collect();

    let mut removed = 0usize;
    let mut kept = 0usize;

    for path in &entries {
        let cp = match Checkpoint::load_from(path) {
            Ok(Some(cp)) => cp,
            Ok(None) => continue,
            Err(e) => {
                eprintln!(
                    "warning: could not read {}: {e}",
                    path.display()
                );
                continue;
            }
        };

        let pending = cp.pending_count();
        let done = cp.done_count();
        let total = pending + done;
        let orphaned = !known_sessions.contains(&cp.session);

        // Determine whether this checkpoint should be removed.
        let should_remove = force
            || (clean && (pending == 0 || orphaned));

        if should_remove {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    let reason = if force {
                        "forced"
                    } else if pending == 0 {
                        "completed"
                    } else {
                        "orphaned"
                    };
                    println!(
                        "removed  {:<20}  {:<8}  {}/{} done  ({})",
                        cp.session,
                        cp.kind.as_str(),
                        done,
                        total,
                        reason,
                    );
                    removed += 1;
                }
                Err(e) => {
                    eprintln!(
                        "error: could not remove {}: {e}",
                        path.display()
                    );
                }
            }
        } else {
            let flag = if orphaned { " [orphaned]" } else { "" };
            println!(
                "{:<20}  {:<8}  {}/{} done  ({} remaining){}",
                cp.session,
                cp.kind.as_str(),
                done,
                total,
                pending,
                flag,
            );
            kept += 1;
        }
    }

    if clean || force {
        println!();
        println!("{removed} removed, {kept} kept");
    } else if kept > 0 {
        println!();
        println!(
            "Use `blink checkpoints --clean` to remove completed and orphaned checkpoints."
        );
        println!(
            "Use `blink checkpoints --force` to remove all checkpoint files."
        );
    }

    Ok(())
}
