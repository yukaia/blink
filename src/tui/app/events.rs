//! Background-task event handlers.
//!
//! `handle_app_event` is the central drain for `AppEvent` — every async
//! task that runs against the app (transport list / connect / view fetch,
//! the transfer dispatcher's forwarder, the host-key oneshots) ends with
//! an `app_event_tx.send(AppEvent::...)`. `handle_transfer_event` is the
//! per-event handler for the dispatcher's stream.
//!
//! Pulled out of `mod.rs` so the App lifecycle (new / run / connect /
//! disconnect) reads without ~500 lines of `match` arms inline. The
//! handlers are `pub(super) fn` because the event-loop dispatcher in
//! mod.rs is their only caller.

use std::sync::Arc;

use tokio::sync::Mutex;
use zeroize::Zeroize;


use crate::preview::{self, FileViewKind};
use crate::transfer::{format_bytes, Direction, Dispatcher, TransferEvent, TransferManager};
use crate::tui::event::AppEvent;
use crate::tui::state::{HostKeyChangedInfo, OverwritePending, PendingHostKey, ViewerKind};

use super::checkpoint_glue::CheckpointFlush;

use super::{App, LogLevel, Pane, Screen};

impl App {
    pub(super) fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Connected(connected) => {
                // Stale guard: user may have cancelled before connect resolved.
                let Some(mut session) = self.pending_session.take() else {
                    drop(connected.transport);
                    return;
                };

                // FTPS TOFU: the verifier observed a new leaf-cert pin.
                // Persist it onto the session so subsequent connects (and
                // dispatcher workers spawned below) require the same cert.
                if let Some(new_pin) = connected.new_cert_pin {
                    session.cert_sha256 = Some(new_pin);
                    if let Err(e) = session.save() {
                        self.push_log(
                            LogLevel::Warn,
                            format!("could not save FTPS cert pin: {e}"),
                        );
                    } else {
                        self.push_log(
                            LogLevel::Info,
                            format!("pinned FTPS certificate for `{}`", session.name),
                        );
                    }
                }

                let transport = connected.transport;
                let remote_dir = session.remote_dir.clone();
                let password = self.pending_password.clone();
                // Successful connect — clear any leftover passphrase state.
                self.passphrase_input.zeroize();
                self.passphrase_error = None;
                self.passphrase_attempted = false;

                // Apply the session's local_dir override, if any. Empty
                // strings and missing fields fall through to the
                // already-initialized default (typically the user's home).
                if let Some(configured) = session.local_dir.as_ref() {
                    if let Some(resolved) = super::resolve_local_dir(configured) {
                        self.local.path = resolved.display().to_string();
                        self.local.cursor = 0;
                        self.local.clear_filter();
                        self.refresh_local_pane();
                    } else {
                        self.push_log(
                            LogLevel::Warn,
                            format!(
                                "session local_dir `{}` not found; \
                                 keeping {}",
                                configured.display(),
                                self.local.path
                            ),
                        );
                    }
                }

                // Spin up the transfer manager + dispatcher for this session.
                // The dispatcher opens its own connections per parallel slot,
                // so it gets a copy of the password (cached on App for the
                // duration of the session).
                let parallelism = session
                    .parallel_downloads
                    .unwrap_or(self.config.general.parallel_downloads);
                let (manager, mut events_rx) = TransferManager::new(parallelism);
                let dispatcher =
                    Dispatcher::spawn(
                        manager.clone(),
                        session.clone(),
                        password,
                        self.app_event_tx.clone(),
                        // Same store the initial connect used, so a
                        // "trust once" is not re-asked per worker.
                        self.pending_trust.clone(),
                    );

                // Forwarder: drain the dispatcher's event stream into the App
                // event channel as `AppEvent::Transfer(...)`.
                let app_tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = events_rx.recv().await {
                        if app_tx.send(AppEvent::Transfer(ev)).is_err() {
                            break;
                        }
                    }
                });

                self.transport = Some(Arc::new(Mutex::new(transport)));
                self.transfer_manager = Some(manager);
                self.dispatcher = Some(dispatcher);
                let is_scp = session.protocol == crate::session::Protocol::Scp;
                self.current_session = Some(session);
                // An ad-hoc connect leaves nothing on disk. Offer to fix that
                // now that the connection has proven it works — waiting until
                // the user goes looking for the session is how "it connected
                // but didn't save" reads as a bug rather than a design.
                let offer_save = std::mem::take(&mut self.pending_session_unsaved);
                self.screen = if offer_save {
                    Screen::OfferSaveSession
                } else {
                    Screen::Main
                };
                self.active_pane = Pane::Remote;
                self.push_log(
                    LogLevel::Success,
                    format!("connected · {parallelism} parallel slot(s)"),
                );
                if is_scp {
                    self.push_log(
                        LogLevel::Warn,
                        "scp:// is routed through SFTP internally; \
                         full file-manager operations are available".into(),
                    );
                }
                self.refresh_remote_pane(remote_dir);
            }
            AppEvent::ConnectFailed(err) => {
                if self.pending_session.is_none() {
                    return; // user already moved on
                }
                self.pending_session = None;
                self.pending_password = None;
                self.password_input.zeroize();
                self.passphrase_input.zeroize();
                self.passphrase_error = None;
                self.passphrase_attempted = false;
                self.screen = Screen::SessionSelect;
                self.push_log(LogLevel::Error, format!("connect failed: {err}"));
            }
            AppEvent::ConnectKeyNeedsPassphrase => {
                // Stale guard: the user may have escaped out before this
                // result came back.
                if self.pending_session.is_none() {
                    return;
                }
                let was_attempted = self.passphrase_attempted;
                self.passphrase_input.zeroize();
                self.passphrase_error = if was_attempted {
                    Some("passphrase incorrect, try again".into())
                } else {
                    None
                };
                self.screen = Screen::KeyPassphrasePrompt;
            }
            AppEvent::Listed { path, entries } => {
                // Discard stale responses (user navigated again before this returned).
                if path != self.remote.path {
                    return;
                }
                self.remote
                    .set_entries(super::build_remote_pane_entries(&entries, &path));
            }
            AppEvent::ListFailed { path, error } => {
                self.push_log(
                    LogLevel::Error,
                    format!("list {path} failed: {error}"),
                );
            }
            AppEvent::LocalListed { path, entries } => {
                // Stale guard: user may have navigated again before the
                // read_dir returned.
                if path != self.local.path {
                    return;
                }
                self.local.set_entries(entries);
            }
            AppEvent::LocalListFailed { path, error } => {
                // Stale guard same as above — don't blame the new path for
                // the old path's failure.
                if path != self.local.path {
                    return;
                }
                self.push_log(
                    LogLevel::Error,
                    format!("local list {path} failed: {error}"),
                );
            }
            AppEvent::Renamed { from, to } => {
                let from_name = from
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&from)
                    .to_string();
                let to_name = to
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&to)
                    .to_string();
                self.push_log(
                    LogLevel::Success,
                    format!("renamed: {from_name} → {to_name}"),
                );
                let path = self.remote.path.clone();
                self.refresh_remote_pane(path);
            }
            AppEvent::RenameFailed { from, to: _, error } => {
                self.push_log(
                    LogLevel::Error,
                    format!("rename {from} failed: {error}"),
                );
            }
            AppEvent::MkdirDone { path } => {
                let name = path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&path)
                    .to_string();
                self.push_log(LogLevel::Success, format!("created: {name}"));
                let dir = self.remote.path.clone();
                self.refresh_remote_pane(dir);
            }
            AppEvent::MkdirFailed { path, error } => {
                let name = path
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&path)
                    .to_string();
                self.push_log(LogLevel::Error, format!("mkdir {name} failed: {error}"));
            }
            AppEvent::Deleted { name } => {
                self.push_log(LogLevel::Success, format!("deleted: {name}"));
                let path = self.remote.path.clone();
                self.refresh_remote_pane(path);
            }
            AppEvent::DeleteFailed { name, error } => {
                self.push_log(
                    LogLevel::Error,
                    format!("delete {name} failed: {error}"),
                );
            }
            AppEvent::WalkComplete {
                plan,
                conflict_indices,
                symlinks_skipped,
                kind,
            } => {
                if symlinks_skipped > 0 {
                    let noun = if symlinks_skipped == 1 { "symlink" } else { "symlinks" };
                    self.push_log(
                        LogLevel::Info,
                        format!("skipped {symlinks_skipped} {noun} during walk"),
                    );
                }
                if conflict_indices.is_empty() {
                    self.dispatch_plan(plan, kind);
                } else {
                    let pending = match kind {
                        Direction::Download => OverwritePending::DownloadPlan {
                            plan,
                            conflict_indices,
                        },
                        Direction::Upload => OverwritePending::UploadPlan {
                            plan,
                            conflict_indices,
                        },
                        Direction::CreateDir => unreachable!(),
                    };
                    self.pending_overwrite = Some(pending);
                    self.screen = Screen::ConfirmOverwrite;
                }
            }
            AppEvent::WalkFailed { error, kind } => {
                let label = match kind {
                    Direction::Download => "downloads",
                    Direction::Upload => "uploads",
                    Direction::CreateDir => unreachable!(),
                };
                self.push_log(
                    LogLevel::Error,
                    format!("preparing {label} failed: {error}"),
                );
            }
            AppEvent::ViewLoaded { name, kind, bytes } => {
                let mut needs_redraw = false;
                if let Some(viewer) = self.viewer.as_mut()
                    && viewer.name == name {
                        viewer.kind = match kind {
                            FileViewKind::Text => {
                                let text = if preview::is_nfo_file(&name) {
                                    preview::decode_cp437(&bytes)
                                } else {
                                    String::from_utf8_lossy(&bytes).into_owned()
                                };
                                let lines: Vec<String> =
                                    text.lines().map(crate::error::sanitize_line).collect();
                                let tokens = super::viewer::tokenize_lines(&name, &lines);
                                // `lines` is dropped here: the tokens carry
                                // the same text, one Vec per line.
                                ViewerKind::Text { tokens, scroll: 0 }
                            }
                            FileViewKind::Image => {
                                // Only enter Image state if a graphics backend
                                // is available; otherwise show a useful
                                // explanation in the viewer.
                                let proto = preview::detect(
                                    self.config.terminal.image_preview,
                                );
                                if matches!(proto, preview::GraphicsProtocol::None)
                                    || preview::backend_for(proto).is_none()
                                {
                                    let term = std::env::var("TERM")
                                        .unwrap_or_else(|_| "<unset>".into());
                                    ViewerKind::Unsupported(format!(
                                        "no supported graphics protocol \
                                         (TERM={term}). \
                                         supported: kitty, ghostty, wezterm, iterm2"
                                    ))
                                } else {
                                    needs_redraw = true;
                                    ViewerKind::Image { bytes }
                                }
                            }
                            FileViewKind::Unsupported(reason) => {
                                ViewerKind::Unsupported(reason)
                            }
                        };
                    }
                if needs_redraw {
                    self.image_needs_redraw = true;
                }
            }
            AppEvent::ViewFailed { name, error } => {
                if let Some(viewer) = self.viewer.as_mut()
                    && viewer.name == name {
                        viewer.kind =
                            ViewerKind::Unsupported(format!("read failed: {error}"));
                    }
                self.push_log(LogLevel::Error, format!("view {name} failed: {error}"));
            }
            AppEvent::Transfer(ev) => self.handle_transfer_event(ev),
            AppEvent::HostKeyUnknown {
                host,
                key_type,
                fingerprint,
                decision_tx,
            } => {
                self.pending_host_key = Some(PendingHostKey {
                    host,
                    key_type,
                    fingerprint,
                    decision_tx: Some(decision_tx),
                });
                // Don't let a second prompt make this modal its own return
                // target — that strands the user on it. (The shared
                // `SessionTrust` means concurrent prompts are now rare, but
                // the state machine shouldn't depend on that.) The dropped
                // `PendingHostKey` above answers its own oneshot with
                // `Reject`, so the connection it belonged to still unwinds.
                if self.screen != Screen::ConfirmHostKey {
                    self.previous_screen = self.screen.clone();
                }
                self.screen = Screen::ConfirmHostKey;
            }
            AppEvent::HostKeyChanged {
                host,
                lookup_host,
                lookup_port,
                stored_key_type,
                presented_key_type,
                fingerprint,
            } => {
                self.push_log(
                    LogLevel::Error,
                    format!(
                        "HOST KEY MISMATCH for {host}: stored {stored_key_type}, \
                         got {presented_key_type} — connection refused"
                    ),
                );
                self.host_key_changed_info = Some(HostKeyChangedInfo {
                    host,
                    lookup_host,
                    lookup_port,
                    stored_key_type,
                    presented_key_type,
                    fingerprint,
                });
                self.pending_session = None;
                self.screen = Screen::HostKeyChanged;
            }
        }
    }

    /// O(1) lookup of a single job by id. The manager retains jobs across
    /// state changes, so this works for Started/Complete/Failed alike.
    fn job_lookup(&self, id: u64) -> Option<crate::transfer::TransferJob> {
        self.transfer_manager.as_ref().and_then(|m| m.job(id))
    }

    pub(super) fn handle_transfer_event(&mut self, ev: TransferEvent) {
        match ev {
            TransferEvent::Queued(job) => {
                self.push_log(LogLevel::Info, format!("queued: {}", job.remote_path));
            }
            TransferEvent::Started(id) => {
                // Flip the in-memory state to `in_progress`; the actual disk
                // write is debounced. A crash before the next flush means the
                // job is re-queued on resume (its file-on-disk state still
                // says `pending`) — which is the same safe outcome as a
                // crash mid-transfer, just at a different moment.
                if let Some((kind, cp_idx)) = self.checkpoint_job_map.get(&id).copied()
                    && let Some(cp) = self.active_checkpoints.get_mut(&kind) {
                        cp.mark_in_progress(cp_idx);
                        if let Err(e) = cp.flush_if_due() {
                            tracing::warn!(id, cp_idx, "checkpoint in_progress flush failed: {e}");
                        }
                    }
                if let Some(j) = self.job_lookup(id) {
                    self.push_log(
                        LogLevel::Info,
                        format!("downloading: {}", j.remote_path),
                    );
                }
            }
            TransferEvent::Progress => {
                // Deliberately empty. Byte counts live in TransferManager and
                // the transfers pane reads them from `active_jobs()` as it
                // renders; the only job this event has is waking the run loop
                // for that redraw. Bursts are collapsed upstream by
                // `event::drain_progress`, so keep this a no-op —
                // anything added here would run once per drained burst, not
                // once per update.
            }
            TransferEvent::Complete(id) => {
                // Update in-memory state and either debounce-save (still more
                // work pending) or delete the file (every job is done).
                if let Some((kind, cp_idx)) = self.checkpoint_job_map.get(&id).copied() {
                    if let Some(cp) = self.active_checkpoints.get_mut(&kind) {
                        cp.mark_done(cp_idx);
                    }
                    // Writes the mark (debounced — one fsync per completed
                    // job would dominate a large batch; a missed mark only
                    // means that job re-runs on resume), and drops the
                    // checkpoint once this direction has nothing left to do
                    // so a later `r` doesn't resume a finished batch. A batch
                    // running the other way keeps its own checkpoint.
                    self.settle_checkpoint(kind, CheckpointFlush::Debounced);
                }
                if let Some(j) = self.job_lookup(id) {
                    self.push_log(
                        LogLevel::Success,
                        format!(
                            "complete: {} ({})",
                            j.remote_path,
                            format_bytes(j.bytes_total)
                        ),
                    );
                    // Uploads land new files on the remote side; refresh the
                    // pane so the user sees them. Skip for downloads — the
                    // local pane doesn't auto-refresh on its own either, and
                    // a flood of small downloads would thrash the listing.
                    if j.direction == crate::transfer::Direction::Upload {
                        let path = self.remote.path.clone();
                        self.refresh_remote_pane(path);
                    }
                }
            }
            TransferEvent::Failed { id, error } => {
                // Evict the job from the checkpoint map regardless of the
                // failure reason. For a "cancelled" failure the single-cancel
                // path already removed it above; this is a belt-and-suspenders
                // guard for transport errors and unexpected failures.
                //
                // We do NOT mark the job `done` in the file — a failed job
                // should be re-queued on resume, not silently skipped.
                let failed_entry = self.checkpoint_job_map.remove(&id);

                // Failed jobs are left as `in_progress` in the checkpoint
                // file (the `Started` write already flipped them from
                // `pending`). On resume they will be re-queued, which is
                // safe: partial downloads are overwritten, mkdir is
                // idempotent. If the batch was explicitly discarded via
                // batch-cancel, `active_checkpoint` is already None.
                //
                // Force-flush here so the most recent batch of Done marks
                // hits disk: the user is likely to react to the failure by
                // killing or quitting blink, and we don't want them losing
                // a debounce-window of completed-job state.
                if let Some((kind, _)) = failed_entry
                    && let Some(cp) = self.active_checkpoints.get_mut(&kind)
                    && let Err(e) = cp.flush()
                {
                    tracing::warn!(id, "checkpoint flush after failure failed: {e}");
                }

                let label = self
                    .job_lookup(id)
                    .map(|j| j.remote_path)
                    .unwrap_or_else(|| format!("id={id}"));
                self.push_log(LogLevel::Error, format!("failed: {label}: {error}"));
            }
            TransferEvent::Paused => {
                self.push_log(LogLevel::Warn, "transfers paused".into());
            }
            TransferEvent::Resumed => {
                self.push_log(LogLevel::Info, "transfers resumed".into());
            }
        }
    }
}
