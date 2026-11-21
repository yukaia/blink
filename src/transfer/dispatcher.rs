//! Pulls pending jobs from [`TransferManager`] and runs them against the
//! transport layer.
//!
//! Workers share a pool of idle connections: a job takes a pooled
//! connection when one is available and opens its own otherwise, and a
//! successful job returns the connection for the next job to reuse. At
//! most `parallel_downloads` worker connections are live at once (one per
//! concurrently running worker), on top of the browsing connection — so a
//! server's per-user connection limit sees one more than the setting.
//! Without the pool, every job — including each mkdir of a recursive
//! upload — paid a full TCP + SSH/TLS handshake and auth round-trip, which
//! dominated batches of small files.
//!
//! A pooled connection idle for [`POOL_IDLE_LIMIT`] is closed, and so are
//! any beyond a lowered `parallel_downloads`: idle connections held open
//! counted against servers' per-user limits (often 2–5 on FTP) long after
//! the batch that opened them.
//!
//! A connection is only pooled after a *successful* job. Failures close
//! it instead: the protocol state after a failed op is uncertain
//! (especially FTP data channels), and reconnecting is cheap relative to
//! debugging a desynced session. A job that fails with
//! [`BlinkError::Disconnected`] is retried once on a fresh connection:
//! idle pooled connections can be reaped by server idle timeouts, and a
//! network blip shouldn't fail a long transfer either — a download picks up
//! from its partial file.
//!
//! Every close here has a deadline ([`CLOSE_TIMEOUT`]): FTP's `QUIT` waits
//! for a reply, and from a server that went away silently it never comes.
//!
//! Pause / resume gates new dispatches; in-flight workers complete naturally.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{self, BlinkError};
use crate::session::Session;
use crate::transfer::{Direction, TransferJob, TransferManager, TransferState};
use crate::transport::{self, ProgressUpdate, Transport};

/// How often the dispatcher loop checks its shutdown flag while it waits.
/// Work wakes it at once (see [`TransferManager::woken`]); this only bounds
/// how long a shutdown takes to notice.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum interval between bytes-per-second recalculations, in ms.
const SPEED_SAMPLE_MS: u128 = 250;

/// How often a transfer's speed is recalculated when no progress arrives,
/// so a stalled transfer falls to 0 rather than showing its last speed.
const STALL_TICK: Duration = Duration::from_secs(1);

/// How long a connection may sit idle in the pool before it's closed.
const POOL_IDLE_LIMIT: Duration = Duration::from_secs(60);

/// How long closing a connection may take before it's abandoned (and
/// dropped, which closes the socket). Quitting waited on it: an idle FTP
/// connection to a server that vanished without a reset waited for a
/// `QUIT` reply until TCP gave up, about 15 minutes on Linux.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Close `transport` politely, but give up after [`CLOSE_TIMEOUT`].
async fn close_within(mut transport: Box<dyn Transport>) {
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, transport.close()).await;
}

use crate::transport::CONNECT_TIMEOUT;

/// Handle to a running dispatcher.
///
/// `join` is an `Option` only so [`Self::shutdown`] can take it: moving a
/// field out of a type that implements `Drop` is not allowed, and the `Drop`
/// impl below is what guarantees the loop stops even when the handle is
/// discarded rather than shut down politely.
pub struct Dispatcher {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Dispatcher {
    /// Stop the loop when the handle goes away.
    ///
    /// Dropping a `JoinHandle` detaches its task rather than aborting it, so
    /// without this the loop kept its flag unset and ran for the life of the
    /// process — polling every [`POLL_INTERVAL`], holding its pooled
    /// connections open, and keeping a `TransferManager` clone alive (which
    /// in turn kept the event channel open, so the forwarder task never
    /// exited either). The `Connected` handler reaches exactly this case: it
    /// reassigns `App::dispatcher`, dropping the previous handle.
    ///
    /// Signalling is all `Drop` can do — it cannot await — and all it needs
    /// to do. The loop notices within one poll interval and still runs its
    /// own teardown, closing pooled connections at the protocol level on the
    /// way out. Aborting the task instead would skip that.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl Dispatcher {
    /// Spawn a dispatcher that runs in the background until [`Self::shutdown`] is
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
    /// the SFTP host-key handler can send events to the TUI.
    ///
    /// `trust` must be the same store the initial connect used. A key the
    /// user accepted with "trust once" is deliberately not written to
    /// known_hosts, so without the shared store every worker connection
    /// would find it unknown and raise its own prompt mid-transfer.
    pub fn spawn(
        manager: TransferManager,
        session: Session,
        password: Option<zeroize::Zeroizing<String>>,
        app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
        trust: crate::known_hosts::SessionTrust,
    ) -> Self {
        Self::spawn_with_pool(manager, session, password, app_event_tx, trust, Pool::new())
    }

    /// [`Self::spawn`] with the connection pool given, so a test can seed it
    /// with connections that need no server.
    fn spawn_with_pool(
        manager: TransferManager,
        session: Session,
        password: Option<zeroize::Zeroizing<String>>,
        app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
        trust: crate::known_hosts::SessionTrust,
        pool: Pool,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let password = password.map(Arc::new);
        let join = tokio::spawn(run_loop(
            manager,
            session,
            password,
            Arc::clone(&shutdown),
            app_event_tx,
            trust,
            Arc::new(pool),
        ));
        Self {
            shutdown,
            join: Some(join),
        }
    }

    /// Stop the dispatcher loop and wait for it to finish. In-flight workers
    /// finish what they're doing; no new jobs will be picked up after this
    /// returns. Idle pooled connections are closed before this resolves.
    ///
    /// Stronger than simply dropping the handle, which signals the loop but
    /// returns immediately — use this where the pool must be closed before
    /// the caller proceeds.
    pub async fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

/// Idle-connection pool shared by every worker slot of one dispatcher.
///
/// A worker takes a connection when its job starts (opening a fresh one
/// when the pool is empty) and returns it after a successful job. At most
/// `parallelism` connections are ever live because at most that many
/// workers run concurrently. [`Pool::evict`] closes what's sat idle too
/// long, or is beyond a lowered parallelism.
struct Pool {
    /// Idle connections and when each was parked, oldest first.
    idle: Mutex<Vec<(Instant, Box<dyn Transport>)>>,
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

    /// The most recently parked connection: the least likely to have been
    /// reaped by the server.
    fn take(&self) -> Option<Box<dyn Transport>> {
        self.idle.lock().pop().map(|(_, t)| t)
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
            idle.push((Instant::now(), transport));
        }
        // else dropped: parallelism shrank below the idle count.
    }

    /// Take out, for closing, every connection parked before
    /// `now - POOL_IDLE_LIMIT`, and the oldest ones beyond `cap`.
    fn evict(&self, now: Instant, cap: usize) -> Vec<Box<dyn Transport>> {
        let mut idle = self.idle.lock();
        let excess = idle.len().saturating_sub(cap);
        let mut out = Vec::new();
        let mut kept = Vec::with_capacity(idle.len());
        for (i, (parked, t)) in std::mem::take(&mut *idle).into_iter().enumerate() {
            if i < excess || now.duration_since(parked) >= POOL_IDLE_LIMIT {
                out.push(t);
            } else {
                kept.push((parked, t));
            }
        }
        *idle = kept;
        out
    }

    /// Mark the pool closed and hand back the idle transports so the caller
    /// can close them cleanly. Subsequent `put`s drop their argument.
    fn close(&self) -> Vec<Box<dyn Transport>> {
        self.closed.store(true, Ordering::Release);
        std::mem::take(&mut *self.idle.lock())
            .into_iter()
            .map(|(_, t)| t)
            .collect()
    }
}

async fn run_loop(
    manager: TransferManager,
    session: Session,
    password: Option<Arc<zeroize::Zeroizing<String>>>,
    shutdown: Arc<AtomicBool>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    trust: crate::known_hosts::SessionTrust,
    pool: Arc<Pool>,
) {
    let active = Arc::new(AtomicU8::new(0));

    // Wait until woken — a job queued or ended, a slot freed — or for the
    // poll interval, whichever is first. The interval is only for noticing
    // a shutdown.
    let idle = || async {
        tokio::select! {
            () = manager.woken() => {}
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    };

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Close what has sat idle too long, or is beyond a lowered limit,
        // off this loop: a close can take up to its deadline.
        let stale = pool.evict(Instant::now(), usize::from(manager.parallelism()));
        if !stale.is_empty() {
            tokio::spawn(async move {
                for t in stale {
                    close_within(t).await;
                }
            });
        }

        // Don't dispatch if paused or already at the concurrency limit.
        if manager.is_paused() || active.load(Ordering::Acquire) >= manager.parallelism() {
            idle().await;
            continue;
        }

        let job = match manager.take_next_pending() {
            Some(j) => j,
            None => {
                idle().await;
                continue;
            }
        };

        active.fetch_add(1, Ordering::AcqRel);
        let guard = ActiveGuard(Arc::clone(&active), manager.clone());
        let job_id = job.id;
        let manager_w = manager.clone();
        let session_w = session.clone();
        let password_w = password.clone();
        let tx_w = app_event_tx.clone();
        let pool_w = Arc::clone(&pool);
        let trust_w = trust.clone();

        // The worker must not reach its completion path before its abort
        // handle is registered. If it did, `deregister_active` would report
        // "already removed" (which `run_one` reads as "cancel won the race")
        // and return without ever marking the job Complete or Failed — the
        // job would sit Active in the UI forever, and the late
        // `register_active` below would leak a handle that nothing removes.
        // Gate the body on a oneshot that fires only after registration.
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            // The guard decrements `active` on drop, even if `run_one` panics.
            let _g = guard;
            // Err means the dispatcher dropped the sender without registering
            // us; nothing would ever mark or cancel this job, so don't start.
            if start_rx.await.is_err() {
                return;
            }
            // A panic would skip `run_one`'s own marking and leave the job
            // Active for good: its destination still taken, its batch never
            // finished, and resume refused as "still in flight".
            let manager_p = manager_w.clone();
            let ran = std::panic::AssertUnwindSafe(run_one(
                manager_w, session_w, password_w, tx_w, pool_w, trust_w, job,
            ))
            .catch_unwind()
            .await;
            if let Err(panic) = ran
                && manager_p.deregister_active(job_id)
            {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown cause".into());
                manager_p.mark(
                    job_id,
                    TransferState::Failed(error::sanitize(format!("internal error: {what}"))),
                );
            }
        });
        manager.register_active(job_id, join.abort_handle());
        // Registration is done — release the worker. A send error means the
        // task was already aborted by `cancel`, which has marked it failed.
        let _ = start_tx.send(());
    }

    // Close idle pooled connections on shutdown. Workers still in flight
    // hold their own transports and close them on their own paths; the
    // pool is marked closed so their late returns are dropped instead of
    // stashed.
    for t in pool.close() {
        close_within(t).await;
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
    trust: &crate::known_hosts::SessionTrust,
) -> crate::error::Result<Box<dyn Transport>> {
    // A stalling server (connects but never completes the handshake) would
    // otherwise pin this worker slot for the lifetime of the TCP session.
    // Workers rarely prompt — the session's trust covers them — but if one
    // does, the user's time with it open is not the server stalling.
    let (user_wait, waiting) = transport::UserWait::new();
    let connected = transport::within_deadline_excluding_waits(
        CONNECT_TIMEOUT,
        waiting,
        transport::open(
            session,
            password,
            app_event_tx.clone(),
            trust.clone(),
            user_wait,
        ),
    )
    .await
    .ok_or_else(|| BlinkError::connect("connection timed out"))??;
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
#[allow(clippy::too_many_arguments)]
async fn run_one(
    manager: TransferManager,
    session: Session,
    password: Option<Arc<zeroize::Zeroizing<String>>>,
    app_event_tx: mpsc::UnboundedSender<crate::tui::event::AppEvent>,
    pool: Arc<Pool>,
    trust: crate::known_hosts::SessionTrust,
    job: TransferJob,
) {
    let id = job.id;

    // Progress channel: the transport pushes raw byte counts; this task
    // smooths them into bytes-per-second and forwards to the manager (which
    // fans them out as `TransferEvent::Progress`).
    let (prog_tx, mut prog_rx) = mpsc::unbounded_channel::<ProgressUpdate>();

    let manager_p = manager.clone();
    let progress_task = tokio::spawn(async move {
        let mut speed = Speed::default();
        let mut tick = tokio::time::interval(STALL_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                update = prog_rx.recv() => {
                    let Some(p) = update else { break };
                    let bps = speed.sample(TokioInstant::now(), p.bytes_done);
                    manager_p.update_progress(id, p.bytes_done, p.bytes_total, bps);
                    speed.total = p.bytes_total;
                }
                _ = tick.tick() => {
                    // Nothing new arrived: report the rate as it now stands,
                    // which falls to 0 for a stalled transfer.
                    if let Some(done) = speed.last_done
                        && speed.stale(TokioInstant::now())
                    {
                        let bps = speed.sample(TokioInstant::now(), done);
                        manager_p.update_progress(id, done, speed.total, bps);
                    }
                }
            }
        }
    });

    // Acquire a connection, run the job, and pool or close the connection.
    let result: crate::error::Result<()> = async {
        let pw = password.as_ref().map(|s| s.as_str());
        let mut transport = match pool.take() {
            Some(t) => t,
            None => connect(&session, pw, &app_event_tx, &trust).await?,
        };

        let mut outcome = run_job(transport.as_mut(), &job, &prog_tx).await;

        // A lost connection gets one more try on a fresh one, whether it
        // came from the pool (a server idle timeout can reap a parked one)
        // or was opened for this job (a network blip). Resume makes the
        // retry cheap: a download picks up from its partial file. Once
        // only, so a server that drops connections on purpose sees each
        // job twice, not forever.
        if matches!(outcome, Err(BlinkError::Disconnected(_))) {
            drop(transport); // dead; Drop tears down the socket
            transport = connect(&session, pw, &app_event_tx, &trust).await?;
            outcome = run_job(transport.as_mut(), &job, &prog_tx).await;
        }

        match outcome {
            Ok(()) => {
                pool.put(transport, usize::from(manager.parallelism()));
                Ok(())
            }
            Err(e) => {
                // Bounded: the job isn't marked failed until this returns,
                // and an unanswered close left it showing as running.
                close_within(transport).await;
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

/// Tokio's clock, which a test can pause and move; the same as the system
/// clock otherwise.
type TokioInstant = tokio::time::Instant;

/// A transfer's speed, sampled at most every [`SPEED_SAMPLE_MS`].
#[derive(Default)]
struct Speed {
    /// When and at what byte count the last sample was taken. `None` until
    /// the first progress update, which is the starting point: a resumed
    /// download reports bytes done from its resume point, and measuring
    /// from 0 counted everything already on disk as moved in the first
    /// quarter-second.
    last: Option<(TokioInstant, u64)>,
    /// The last byte count reported, for re-sampling when none arrives.
    last_done: Option<u64>,
    /// The last total reported.
    total: u64,
    /// The last rate computed.
    bps: u64,
}

impl Speed {
    /// The rate at `now` with `done` bytes transferred.
    fn sample(&mut self, now: TokioInstant, done: u64) -> u64 {
        self.last_done = Some(done);
        let Some((t, b)) = self.last else {
            self.last = Some((now, done));
            return 0;
        };
        let elapsed = now.duration_since(t);
        if elapsed.as_millis() >= SPEED_SAMPLE_MS {
            self.bps = (done.saturating_sub(b) as f64 / elapsed.as_secs_f64()) as u64;
            self.last = Some((now, done));
        }
        self.bps
    }

    /// Whether the rate is due a recalculation for lack of updates.
    fn stale(&self, now: TokioInstant) -> bool {
        self.last
            .is_some_and(|(t, _)| now.duration_since(t).as_millis() >= SPEED_SAMPLE_MS)
    }
}

/// Decrements the active-worker counter on drop, and wakes the dispatcher
/// to fill the freed slot. Catches panics inside the worker task in
/// addition to the normal completion path.
///
/// The wake has to come from here, not only from the job's final mark: the
/// mark lands while the slot is still counted, so a dispatcher woken by it
/// alone could look, find no room, and wait out the poll interval.
struct ActiveGuard(Arc<AtomicU8>, TransferManager);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
        self.1.wake();
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

    // -- teardown ----------------------------------------------------------

    fn idle_session() -> Session {
        // The loop never connects: nothing is ever queued, so host and port
        // are never used.
        Session {
            name: "idle".to_string(),
            protocol: crate::session::Protocol::Sftp,
            host: "127.0.0.1".to_string(),
            port: 1,
            username: "u".to_string(),
            remote_dir: "/".to_string(),
            local_dir: None,
            auth: crate::session::AuthMethod::Password,
            parallel_downloads: None,
            theme: None,
            accept_invalid_certs: false,
            cert_sha256: None,
        }
    }

    /// Dropping the handle must stop the loop.
    ///
    /// `shutdown()` is the polite path, but it is not the only one: the
    /// `Connected` handler reassigns `App::dispatcher`, which drops the
    /// previous handle. Without a `Drop` impl the loop never sees its flag
    /// set, so it polls every 100ms for the life of the process while
    /// holding its pooled connections and a `TransferManager` clone.
    ///
    /// Observed through the event channel: the loop owns the last manager
    /// clone once the caller drops theirs, so the channel closes only when
    /// the loop actually returns.
    #[tokio::test]
    async fn dropping_the_handle_stops_the_loop() {
        let (manager, mut events_rx) = TransferManager::new(1);
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let dispatcher = Dispatcher::spawn(
            manager.clone(),
            idle_session(),
            None,
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
        );

        drop(manager); // the App's copy
        drop(dispatcher); // must signal the loop, not just detach

        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while events_rx.recv().await.is_some() {}
        })
        .await;

        assert!(
            drained.is_ok(),
            "loop still running after its handle was dropped",
        );
    }

    /// A freed slot is filled at once. The loop used to poll every 100 ms
    /// when every slot was busy, so each job of a batch was followed by
    /// half that, on average, of nothing. On a paused clock the old loop
    /// took 100 ms of virtual time per job here.
    #[tokio::test(start_paused = true)]
    async fn a_freed_slot_is_filled_without_waiting_for_a_poll() {
        let (manager, _events_rx) = TransferManager::new(1);
        let pool = Pool::new();
        pool.put(boxed(), 1);
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let dispatcher = Dispatcher::spawn_with_pool(
            manager.clone(),
            idle_session(),
            None,
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
            pool,
        );
        let ids: Vec<u64> = (0..20)
            .map(|i| manager.enqueue_mkdir(format!("/d{i}")).unwrap())
            .collect();

        let started = tokio::time::Instant::now();
        while ids
            .iter()
            .any(|&id| manager.job(id).unwrap().state != TransferState::Complete)
        {
            // Lets the paused clock advance, so the old loop's polls showed.
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert!(
                started.elapsed() < POLL_INTERVAL,
                "20 jobs took {:?} of virtual time",
                started.elapsed()
            );
        }
        dispatcher.shutdown().await;
    }

    /// `shutdown()` keeps its stronger guarantee: it returns only once the
    /// loop has actually finished, so idle pooled connections are closed
    /// before the caller proceeds.
    #[tokio::test]
    async fn shutdown_still_waits_for_the_loop_to_finish() {
        let (manager, mut events_rx) = TransferManager::new(1);
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let dispatcher = Dispatcher::spawn(
            manager.clone(),
            idle_session(),
            None,
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
        );

        dispatcher.shutdown().await;
        drop(manager);

        // The loop is already done, so this drains immediately.
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while events_rx.recv().await.is_some() {}
        })
        .await;
        assert!(drained.is_ok(), "shutdown must leave no loop behind");
    }

    /// A connection whose mkdir panics, as a bug in a transport would.
    struct PanickingTransport;

    #[async_trait::async_trait]
    impl Transport for PanickingTransport {
        fn protocol(&self) -> crate::session::Protocol {
            crate::session::Protocol::Sftp
        }
        async fn list(&mut self, _: &str) -> crate::error::Result<transport::Listing> {
            unreachable!()
        }
        async fn download(
            &mut self,
            _: &str,
            _: &std::path::Path,
            _: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn upload(
            &mut self,
            _: &std::path::Path,
            _: &str,
            _: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn rename(&mut self, _: &str, _: &str) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn rename_over(&mut self, _: &str, _: &str) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn delete_file(&mut self, _: &str) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn delete_dir(&mut self, _: &str, _: bool) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn mkdir(&mut self, _: &str) -> crate::error::Result<()> {
            panic!("transport bug");
        }
        async fn metadata(
            &mut self,
            _: &str,
        ) -> crate::error::Result<Option<transport::RemoteEntry>> {
            unreachable!()
        }
        async fn read_to_bytes(&mut self, _: &str, _: u64) -> crate::error::Result<bytes::Bytes> {
            unreachable!()
        }
        async fn close(&mut self) -> crate::error::Result<()> {
            Ok(())
        }
    }

    /// A worker that panics fails its job. It used to leave it Active for
    /// good: its destination stayed taken, its batch never finished, and
    /// `r` was refused for the rest of the session as "still in flight".
    #[tokio::test]
    async fn a_worker_that_panics_fails_its_job() {
        // Two slots for the two pooled connections: with one, the second
        // would be closed as beyond the limit before it was used.
        let (manager, _events_rx) = TransferManager::new(2);
        let pool = Pool::new();
        pool.put(boxed(), 2);
        pool.put(Box::new(PanickingTransport), 2);
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        let dispatcher = Dispatcher::spawn_with_pool(
            manager.clone(),
            idle_session(),
            None,
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
            pool,
        );
        let first = manager.enqueue_mkdir("/a".into()).unwrap();
        let second = manager.enqueue_mkdir("/b".into()).unwrap();

        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let states = [first, second].map(|id| manager.job(id).unwrap().state);
                if states
                    .iter()
                    .all(|s| !matches!(s, TransferState::Pending | TransferState::Active))
                {
                    return states;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both jobs end");

        match &settled[0] {
            TransferState::Failed(why) => assert!(why.contains("transport bug"), "{why}"),
            other => panic!("the panicking job should fail, not {other:?}"),
        }
        assert_eq!(
            settled[1],
            TransferState::Complete,
            "the next job still runs"
        );
        dispatcher.shutdown().await;
    }

    // -- audit fixes ---------------------------------------------------------

    /// A connection for tests: mkdir answers with `mkdir`, a download sends
    /// `progress` updates (one per entry, after its delay) and then waits
    /// for good, and close never returns if `close_hangs`.
    struct Scripted {
        mkdir: fn() -> crate::error::Result<()>,
        progress: Vec<(Duration, u64)>,
        close_hangs: bool,
    }

    impl Scripted {
        fn new() -> Self {
            Self {
                mkdir: || Ok(()),
                progress: Vec::new(),
                close_hangs: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Transport for Scripted {
        fn protocol(&self) -> crate::session::Protocol {
            crate::session::Protocol::Ftp
        }
        async fn list(&mut self, _: &str) -> crate::error::Result<transport::Listing> {
            unreachable!()
        }
        async fn download(
            &mut self,
            _: &str,
            _: &std::path::Path,
            progress: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> crate::error::Result<()> {
            let progress = progress.unwrap();
            for (delay, done) in self.progress.clone() {
                tokio::time::sleep(delay).await;
                let _ = progress.send(ProgressUpdate {
                    bytes_done: done,
                    bytes_total: 10_000_000_000,
                });
            }
            std::future::pending().await
        }
        async fn upload(
            &mut self,
            _: &std::path::Path,
            _: &str,
            _: Option<mpsc::UnboundedSender<ProgressUpdate>>,
        ) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn rename(&mut self, _: &str, _: &str) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn rename_over(&mut self, _: &str, _: &str) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn delete_file(&mut self, _: &str) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn delete_dir(&mut self, _: &str, _: bool) -> crate::error::Result<()> {
            unreachable!()
        }
        async fn mkdir(&mut self, _: &str) -> crate::error::Result<()> {
            (self.mkdir)()
        }
        async fn metadata(
            &mut self,
            _: &str,
        ) -> crate::error::Result<Option<transport::RemoteEntry>> {
            unreachable!()
        }
        async fn read_to_bytes(&mut self, _: &str, _: u64) -> crate::error::Result<bytes::Bytes> {
            unreachable!()
        }
        async fn close(&mut self) -> crate::error::Result<()> {
            if self.close_hangs {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
    }

    fn spawn_with(manager: &TransferManager, pool: Pool) -> Dispatcher {
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();
        Dispatcher::spawn_with_pool(
            manager.clone(),
            idle_session(),
            None,
            ev_tx,
            crate::known_hosts::SessionTrust::new(),
            pool,
        )
    }

    /// An FTP `QUIT` to a server that vanished waited for a reply until TCP
    /// gave up — about 15 minutes, with quitting blink waiting on it.
    #[tokio::test(start_paused = true)]
    async fn shutdown_doesnt_wait_on_a_connection_that_wont_close() {
        let (manager, _rx) = TransferManager::new(1);
        let pool = Pool::new();
        pool.put(
            Box::new(Scripted {
                close_hangs: true,
                ..Scripted::new()
            }),
            1,
        );
        let dispatcher = spawn_with(&manager, pool);

        let started = tokio::time::Instant::now();
        dispatcher.shutdown().await;
        assert!(started.elapsed() <= CLOSE_TIMEOUT + POLL_INTERVAL * 2);
    }

    /// The failed job was marked only once its connection had closed, so a
    /// close that never returned left it showing as running.
    #[tokio::test(start_paused = true)]
    async fn a_failed_job_is_marked_failed_even_if_its_connection_wont_close() {
        let (manager, _rx) = TransferManager::new(1);
        let pool = Pool::new();
        pool.put(
            Box::new(Scripted {
                mkdir: || Err(BlinkError::permission("no")),
                close_hangs: true,
                ..Scripted::new()
            }),
            1,
        );
        let dispatcher = spawn_with(&manager, pool);
        let id = manager.enqueue_mkdir("/a".into()).unwrap();

        tokio::time::sleep(CLOSE_TIMEOUT + Duration::from_secs(1)).await;
        assert!(
            matches!(manager.job(id).unwrap().state, TransferState::Failed(ref m) if m.contains("no")),
            "{:?}",
            manager.job(id).unwrap().state
        );
        dispatcher.shutdown().await;
    }

    /// A lost connection gets one more try on a fresh one. (Pooled or
    /// opened for the job, it's the same path now; here the fresh one
    /// can't connect, which is how the retry shows.)
    #[tokio::test]
    async fn a_lost_connection_is_retried_once_on_a_fresh_one() {
        let (manager, _rx) = TransferManager::new(1);
        let pool = Pool::new();
        pool.put(
            Box::new(Scripted {
                mkdir: || Err(BlinkError::disconnected("reset")),
                ..Scripted::new()
            }),
            1,
        );
        let dispatcher = spawn_with(&manager, pool);
        let id = manager.enqueue_mkdir("/a".into()).unwrap();

        let state = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let s = manager.job(id).unwrap().state;
                if matches!(s, TransferState::Failed(_)) {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(&state, TransferState::Failed(m) if m.contains("connection failed")),
            "the retry's connect is what failed: {state:?}"
        );
        dispatcher.shutdown().await;
    }

    // -- speed --------------------------------------------------------------

    /// Measured from 0, a download resumed at 3.9 GB showed ~15 GB/s.
    #[test]
    fn a_resumed_transfers_speed_counts_from_where_it_resumed() {
        let t0 = TokioInstant::now();
        let mut speed = Speed::default();
        assert_eq!(speed.sample(t0, 3_900_000_000), 0);
        let bps = speed.sample(t0 + Duration::from_millis(500), 3_905_000_000);
        assert_eq!(bps, 10_000_000);
    }

    /// The speed was recalculated only when progress arrived, so a stalled
    /// transfer kept showing its last healthy speed.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_transfers_speed_falls_to_zero() {
        let (manager, _rx) = TransferManager::new(1);
        let pool = Pool::new();
        pool.put(
            Box::new(Scripted {
                progress: vec![(Duration::ZERO, 0), (Duration::from_millis(500), 5_000_000)],
                ..Scripted::new()
            }),
            1,
        );
        let dispatcher = spawn_with(&manager, pool);
        let id = manager
            .enqueue_download("/r".into(), std::path::PathBuf::from("/l"))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(manager.job(id).unwrap().bytes_per_sec > 0, "moving");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(manager.job(id).unwrap().bytes_per_sec, 0, "stalled");
        assert_eq!(manager.job(id).unwrap().bytes_done, 5_000_000);

        manager.cancel(id);
        dispatcher.shutdown().await;
    }

    // -- the pool -----------------------------------------------------------

    #[test]
    fn idle_connections_are_evicted_after_the_limit() {
        let pool = Pool::new();
        pool.put(boxed(), 4);
        let now = Instant::now();
        assert!(pool.evict(now, 4).is_empty(), "fresh: kept");
        assert_eq!(
            pool.evict(now + POOL_IDLE_LIMIT, 4).len(),
            1,
            "stale: evicted"
        );
        assert!(pool.take().is_none());
    }

    /// Lowering the limit didn't trim the pool; connections idle above it
    /// lingered until used.
    #[test]
    fn connections_beyond_a_lowered_limit_are_evicted_oldest_first() {
        let pool = Pool::new();
        for _ in 0..3 {
            pool.put(boxed(), 4);
        }
        pool.put(Box::new(Scripted::new()), 4);
        assert_eq!(pool.evict(Instant::now(), 1).len(), 3);
        let left = pool.take().unwrap();
        assert_eq!(
            left.protocol(),
            crate::session::Protocol::Ftp,
            "the newest stays"
        );
    }
}
