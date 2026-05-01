//! Pulls pending jobs from [`TransferManager`] and runs them against the
//! transport layer.
//!
//! Each parallel slot opens its own connection. For SFTP that's N SSH
//! handshakes per session — fine at N ≤ 10 (the configured ceiling). The
//! alternative — one connection serialised across all jobs — would give you
//! queueing without real parallelism, which defeats the purpose of the
//! `parallel_downloads` knob.
//!
//! Pause / resume gates new dispatches; in-flight workers complete naturally.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{self, BlinkError};
use crate::session::Session;
use crate::transfer::{TransferJob, TransferManager, TransferState};
use crate::transport::{self, ProgressUpdate};

/// How often the dispatcher loop wakes when there's nothing to do.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum interval between bytes-per-second recalculations, in ms.
const SPEED_SAMPLE_MS: u128 = 250;

/// Maximum time allowed for `transport::open` (TCP connect + SSH handshake +
/// auth). A server that accepts the TCP connection but stalls the handshake
/// can otherwise pin a worker slot indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Handle to a running dispatcher.
pub struct Dispatcher {
    shutdown: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl Dispatcher {
    /// Spawn a dispatcher that runs in the background until [`shutdown`] is
    /// called.
    ///
    /// Adjusting concurrency at runtime via
    /// [`TransferManager::set_parallelism`] is honoured on the next loop
    /// iteration — no restart needed.
    ///
    /// `password` is optional and shared (via `Arc`) across all worker tasks.
    /// It is reused for both password auth and as the SSH key passphrase.
    ///
    /// `app_event_tx` is forwarded to each worker's transport connection so
    /// the SFTP host-key handler can send events to the TUI. In practice the
    /// host key is already in known_hosts after the initial connect, so the
    /// channel is rarely used from here — but it must be valid.
    pub fn spawn(
        manager: TransferManager,
        session: Session,
        password: Option<String>,
        app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let password = password.map(Arc::new);
        let join = tokio::spawn(run_loop(
            manager,
            session,
            password,
            Arc::clone(&shutdown),
            app_event_tx,
        ));
        Self { shutdown, join }
    }

    /// Stop the dispatcher loop. In-flight workers finish what they're doing;
    /// no new jobs will be picked up after this returns.
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.join.await;
    }
}

async fn run_loop(
    manager: TransferManager,
    session: Session,
    password: Option<Arc<String>>,
    shutdown: Arc<AtomicBool>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
) {
    let active = Arc::new(AtomicU8::new(0));

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Don't dispatch if paused or already at the concurrency limit.
        if manager.is_paused() || active.load(Ordering::Acquire) >= manager.parallelism() {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let job = match manager.take_next_pending() {
            Some(j) => j,
            None => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };

        active.fetch_add(1, Ordering::AcqRel);
        let guard = ActiveGuard(Arc::clone(&active));
        let job_id = job.id;
        let manager_w = manager.clone();
        let session_w = session.clone();
        let password_w = password.clone();
        let tx_w = app_event_tx.clone();

        let join = tokio::spawn(async move {
            // The guard decrements `active` on drop, even if `run_one` panics.
            let _g = guard;
            run_one(manager_w, session_w, password_w, tx_w, job).await;
        });
        manager.register_active(job_id, join.abort_handle());
    }
}

/// Run a single transfer to completion (or failure). Owns its own connection.
async fn run_one(
    manager: TransferManager,
    session: Session,
    password: Option<Arc<String>>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    job: TransferJob,
) {
    let id = job.id;

    // Progress channel: the transport pushes raw byte counts; this task
    // smooths them into bytes-per-second and forwards to the manager (which
    // fans them out as `TransferEvent::Progress`).
    let (prog_tx, mut prog_rx) = mpsc::unbounded_channel::<ProgressUpdate>();

    let manager_p = manager.clone();
    let progress_task = tokio::spawn(async move {
        let mut last_t = Instant::now();
        let mut last_b: u64 = 0;
        let mut last_bps: u64 = 0;
        loop {
            let Some(p) = prog_rx.recv().await else { break };
            let now = Instant::now();
            let elapsed = now.duration_since(last_t);
            let bps = if elapsed.as_millis() >= SPEED_SAMPLE_MS {
                let delta = p.bytes_done.saturating_sub(last_b);
                let v = (delta as f64 / elapsed.as_secs_f64()) as u64;
                last_t = now;
                last_b = p.bytes_done;
                last_bps = v;
                v
            } else {
                last_bps
            };
            manager_p.update_progress(id, p.bytes_done, p.bytes_total, bps);
        }
    });

    // Connect, transfer, close. `prog_tx` is moved into the inner block so it
    // drops when the block ends — that's what closes the progress channel and
    // lets the forwarder task exit cleanly.
    let result: crate::error::Result<()> = async move {
        let pw = password.as_ref().map(|s| s.as_str());
        // A stalling server (connects but never completes the SSH handshake)
        // would otherwise pin this worker slot for the lifetime of the TCP
        // session. Enforce a hard deadline on connect + auth.
        let mut transport = tokio::time::timeout(
            CONNECT_TIMEOUT,
            transport::open(&session, pw, app_event_tx),
        )
        .await
        .map_err(|_| BlinkError::connect("connection timed out"))??;
        let outcome = match job.direction {
            crate::transfer::Direction::Download => {
                transport
                    .download(&job.remote_path, &job.local_path, Some(prog_tx))
                    .await
            }
            crate::transfer::Direction::Upload => {
                transport
                    .upload(&job.local_path, &job.remote_path, Some(prog_tx))
                    .await
            }
            crate::transfer::Direction::CreateDir => {
                // mkdir has no progress; drop the sender so the forwarder
                // task exits cleanly.
                drop(prog_tx);
                transport.mkdir(&job.remote_path).await
            }
        };
        // Always close, even if the transfer errored out.
        let _ = transport.close().await;
        outcome
    }
    .await;

    let _ = progress_task.await;

    // Deregister before marking. If cancel() won the race, the entry's
    // already gone — and our final mark would clobber the "cancelled"
    // state that cancel() already wrote.
    if !manager.deregister_active(id) {
        return;
    }

    match result {
        Ok(()) => manager.mark(id, TransferState::Complete),
        // Sanitize before storing: BlinkError::Other wraps arbitrary anyhow
        // chains whose inner messages may not have passed through a sanitizing
        // constructor and could contain server-controlled text.
        Err(e) => manager.mark(id, TransferState::Failed(error::sanitize(e.to_string()))),
    }
}

/// Decrements the active-worker counter on drop. Catches panics inside the
/// worker task in addition to the normal completion path.
struct ActiveGuard(Arc<AtomicU8>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
