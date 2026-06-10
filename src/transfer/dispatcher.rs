//! Pulls pending jobs from [`TransferManager`] and runs them against the
//! transport layer.
//!
//! Workers share a pool of idle connections: a job takes a pooled
//! connection when one is available and opens its own otherwise, and a
//! successful job returns the connection for the next job to reuse. At
//! most `parallel_downloads` connections are live at once (one per
//! concurrently running worker). Without the pool, every job — including
//! each mkdir of a recursive upload — paid a full TCP + SSH/TLS handshake
//! and auth round-trip, which dominated batches of small files.
//!
//! A connection is only pooled after a *successful* job. Failures close
//! it instead: the protocol state after a failed op is uncertain
//! (especially FTP data channels), and reconnecting is cheap relative to
//! debugging a desynced session. A job that fails with
//! [`BlinkError::Disconnected`] on a *reused* connection is retried once
//! on a fresh one — idle pooled connections can be reaped by server idle
//! timeouts, and that must not fail the job.
//!
//! Pause / resume gates new dispatches; in-flight workers complete naturally.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{self, BlinkError};
use crate::session::Session;
use crate::transfer::{Direction, TransferJob, TransferManager, TransferState};
use crate::transport::{self, ProgressUpdate, Transport};

/// How often the dispatcher loop wakes when there's nothing to do.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum interval between bytes-per-second recalculations, in ms.
const SPEED_SAMPLE_MS: u128 = 250;

use crate::transport::CONNECT_TIMEOUT;

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
    /// It's wrapped in `Zeroizing` so the underlying allocation is wiped
    /// when the last reference drops — i.e., when the dispatcher shuts
    /// down. Reused for both password auth and as the SSH key passphrase.
    ///
    /// `app_event_tx` is forwarded to each worker's transport connection so
    /// the SFTP host-key handler can send events to the TUI. In practice the
    /// host key is already in known_hosts after the initial connect, so the
    /// channel is rarely used from here — but it must be valid.
    pub fn spawn(
        manager: TransferManager,
        session: Session,
        password: Option<zeroize::Zeroizing<String>>,
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
    /// no new jobs will be picked up after this returns. Idle pooled
    /// connections are closed before this resolves.
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.join.await;
    }
}

/// Idle-connection pool shared by every worker slot of one dispatcher.
///
/// A worker takes a connection when its job starts (opening a fresh one
/// when the pool is empty) and returns it after a successful job. At most
/// `parallelism` connections are ever live because at most that many
/// workers run concurrently; the put-side cap only matters when the user
/// shrinks parallelism mid-session.
struct Pool {
    idle: Mutex<Vec<Box<dyn Transport>>>,
    /// Set on dispatcher shutdown: late returns from in-flight workers are
    /// dropped (the socket closes via Drop) instead of stashed forever.
    closed: AtomicBool,
}

impl Pool {
    fn new() -> Self {
        Self {
            idle: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    fn take(&self) -> Option<Box<dyn Transport>> {
        self.idle.lock().pop()
    }

    /// Park a healthy connection for the next job. Drops it (closing the
    /// socket via Drop) when the pool is closed or already holds `cap`
    /// idle connections.
    fn put(&self, transport: Box<dyn Transport>, cap: usize) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut idle = self.idle.lock();
        if idle.len() < cap {
            idle.push(transport);
        }
        // else dropped: parallelism shrank below the idle count.
    }

    /// Mark the pool closed and hand back the idle transports so the caller
    /// can close them cleanly. Subsequent `put`s drop their argument.
    fn close(&self) -> Vec<Box<dyn Transport>> {
        self.closed.store(true, Ordering::Release);
        std::mem::take(&mut *self.idle.lock())
    }
}

async fn run_loop(
    manager: TransferManager,
    session: Session,
    password: Option<Arc<zeroize::Zeroizing<String>>>,
    shutdown: Arc<AtomicBool>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
) {
    let active = Arc::new(AtomicU8::new(0));
    let pool = Arc::new(Pool::new());

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
        let pool_w = Arc::clone(&pool);

        let join = tokio::spawn(async move {
            // The guard decrements `active` on drop, even if `run_one` panics.
            let _g = guard;
            run_one(manager_w, session_w, password_w, tx_w, pool_w, job).await;
        });
        manager.register_active(job_id, join.abort_handle());
    }

    // Close idle pooled connections on shutdown. Workers still in flight
    // hold their own transports and close them on their own paths; the
    // pool is marked closed so their late returns are dropped instead of
    // stashed.
    for mut t in pool.close() {
        let _ = t.close().await;
    }
}

/// Open a fresh transport for `session` under the shared connect deadline.
///
/// Workers always inherit the session's pinned FTPS cert (set by the
/// initial UI connect); they don't re-TOFU, so the captured pin is
/// discarded.
async fn connect(
    session: &Session,
    password: Option<&str>,
    app_event_tx: &mpsc::UnboundedSender<crate::tui::event::AppEvent>,
) -> crate::error::Result<Box<dyn Transport>> {
    // A stalling server (connects but never completes the handshake) would
    // otherwise pin this worker slot for the lifetime of the TCP session.
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        transport::open(session, password, app_event_tx.clone()),
    )
    .await
    .map_err(|_| BlinkError::connect("connection timed out"))??;
    Ok(connected.transport)
}

/// Run `job` against `transport`. Download / upload get a clone of the
/// progress sender; mkdir has no progress to report.
async fn run_job(
    transport: &mut dyn Transport,
    job: &TransferJob,
    progress: &mpsc::UnboundedSender<ProgressUpdate>,
) -> crate::error::Result<()> {
    match job.direction {
        Direction::Download => {
            transport
                .download(&job.remote_path, &job.local_path, Some(progress.clone()))
                .await
        }
        Direction::Upload => {
            transport
                .upload(&job.local_path, &job.remote_path, Some(progress.clone()))
                .await
        }
        Direction::CreateDir => transport.mkdir(&job.remote_path).await,
    }
}

/// Run a single transfer to completion (or failure), reusing a pooled
/// connection when one is available.
async fn run_one(
    manager: TransferManager,
    session: Session,
    password: Option<Arc<zeroize::Zeroizing<String>>>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    pool: Arc<Pool>,
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

    // Acquire a connection, run the job, and pool or close the connection.
    let result: crate::error::Result<()> = async {
        let pw = password.as_ref().map(|s| s.as_str());
        let pooled = pool.take();
        let reused = pooled.is_some();
        let mut transport = match pooled {
            Some(t) => t,
            None => connect(&session, pw, &app_event_tx).await?,
        };

        let mut outcome = run_job(transport.as_mut(), &job, &prog_tx).await;

        // An idle pooled connection may have died while parked (server idle
        // timeout, network drop). That must not fail the job: retry once on
        // a fresh connection. Resume support makes the retry cheap — a
        // partially written download picks up from its `.part` offset.
        if reused && matches!(outcome, Err(BlinkError::Disconnected(_))) {
            drop(transport); // dead; Drop tears down the socket
            transport = connect(&session, pw, &app_event_tx).await?;
            outcome = run_job(transport.as_mut(), &job, &prog_tx).await;
        }

        match outcome {
            Ok(()) => {
                pool.put(transport, usize::from(manager.parallelism()));
                Ok(())
            }
            Err(e) => {
                let _ = transport.close().await;
                Err(e)
            }
        }
    }
    .await;

    // The job block above only borrows `prog_tx` (run_job clones it per
    // attempt). Drop the original now to close the progress channel —
    // otherwise the forwarder never exits and the await below deadlocks.
    drop(prog_tx);
    let _ = progress_task.await;

    // Deregister before marking. If cancel() won the race, the entry's
    // already gone — and our final mark would clobber the "cancelled"
    // state that cancel() already wrote.
    if !manager.deregister_active(id) {
        return;
    }

    match result {
        Ok(()) => manager.mark(id, TransferState::Complete),
        // Defense-in-depth: re-sanitize at the storage boundary. Every BlinkError
        // constructor already sanitizes its payload, but `Io(e)` and any future
        // variant whose Display could pull in unsanitized text would slip past
        // otherwise — and this state string ends up rendered in the TUI.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::MockTransport;

    fn boxed() -> Box<dyn Transport> {
        Box::new(MockTransport::new())
    }

    #[test]
    fn pool_take_from_empty_is_none() {
        let pool = Pool::new();
        assert!(pool.take().is_none());
    }

    #[test]
    fn pool_put_then_take_roundtrip() {
        let pool = Pool::new();
        pool.put(boxed(), 2);
        assert!(pool.take().is_some());
        assert!(pool.take().is_none());
    }

    #[test]
    fn pool_put_respects_cap() {
        let pool = Pool::new();
        pool.put(boxed(), 1);
        pool.put(boxed(), 1); // over cap — dropped
        assert!(pool.take().is_some());
        assert!(pool.take().is_none());
    }

    #[test]
    fn pool_closed_drops_puts() {
        let pool = Pool::new();
        let drained = pool.close();
        assert!(drained.is_empty());
        pool.put(boxed(), 4);
        assert!(pool.take().is_none());
    }

    #[test]
    fn pool_close_hands_back_idle_connections() {
        let pool = Pool::new();
        pool.put(boxed(), 4);
        pool.put(boxed(), 4);
        assert_eq!(pool.close().len(), 2);
    }
}
