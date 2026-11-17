//! Transfer-batch controls and theme cycling.
//!
//! Bundles the small one-shot actions triggered from the Main screen
//! key bindings:
//!
//! - `c` — `request_cancel_selected_transfer` (single-job cancel modal)
//! - `C` — `request_cancel_selected_batch` (whole-batch cancel modal,
//!   falls through to single if the job isn't part of a batch)
//! - `p` — `toggle_pause`
//! - `t` — `cycle_theme`
//! - `active_jobs` — small read-only helper the Transfers pane and the
//!   cancel helpers consume; lives here because the cancels are its
//!   primary callers.

use crate::theme::Theme;
use crate::transfer::TransferJob;
use crate::tui::state::PendingCancel;

use super::{App, LogLevel, Screen, name_for_job};

impl App {
    pub(super) fn request_cancel_selected_transfer(&mut self) {
        let jobs = self.active_jobs();
        if jobs.is_empty() {
            return;
        }
        let idx = self.transfer_cursor.min(jobs.len() - 1);
        let job = &jobs[idx];
        self.pending_cancel = Some(PendingCancel::Single {
            id: job.id,
            name: name_for_job(job),
        });
        self.previous_screen = Screen::Main;
        self.screen = Screen::ConfirmCancel;
    }

    /// Cancel every job in the batch the cursor item belongs to. If the
    /// cursor job has no batch_id (single-file enqueue), this falls back
    /// to the single-job cancel modal.
    pub(super) fn request_cancel_selected_batch(&mut self) {
        let manager = match self.transfer_manager.as_ref() {
            Some(m) => m,
            None => return,
        };
        let active_jobs = self.active_jobs();
        if active_jobs.is_empty() {
            return;
        }
        let idx = self.transfer_cursor.min(active_jobs.len() - 1);
        let cursor_job = &active_jobs[idx];

        let Some(batch_id) = cursor_job.batch_id else {
            // Not a batched job — fall through to the single-job modal so
            // the gesture still does something useful.
            self.request_cancel_selected_transfer();
            return;
        };

        // Count siblings in the batch, including pending ones (which the
        // active-only list above doesn't include).
        let (active, pending) = manager.batch_counts(batch_id);
        if active == 0 && pending == 0 {
            return;
        }

        self.pending_cancel = Some(PendingCancel::Batch {
            batch_id,
            active,
            pending,
            cursor_name: name_for_job(cursor_job),
        });
        self.previous_screen = Screen::Main;
        self.screen = Screen::ConfirmCancel;
    }

    pub(super) fn toggle_pause(&mut self) {
        let Some(manager) = &self.transfer_manager else {
            self.push_log(LogLevel::Warn, "not connected".into());
            return;
        };
        if manager.is_paused() {
            manager.resume();
        } else {
            manager.pause();
        }
        // The Paused / Resumed log lines are emitted from
        // handle_transfer_event when the dispatcher echoes the event back.
    }

    /// Cycle to the next theme in `Theme::list_all_names`. Built-ins come
    /// first alphabetically, then user themes (deduplicated by name). The
    /// new theme is applied immediately and persisted to `config.ini` so
    /// it survives a restart.
    ///
    /// Both the position lookup and the persisted value use the theme's
    /// *key* — the name `Theme::load` accepts, which is a built-in name or a
    /// user theme's filename stem — rather than `Theme.name`, which is the
    /// display label from the file's optional `[theme] name` field.
    ///
    /// Those are not the same string, and conflating them broke two things.
    /// `list_all_names` yields keys, so a user theme whose display name
    /// differed from its filename never matched and the cycle jumped back to
    /// index 0 instead of advancing. Worse, the persisted value was the
    /// display name: cycling onto `custom.ini` containing
    /// `[theme] name = My Cool Theme` wrote `theme = My Cool Theme` into
    /// config.ini, which resolves to no file and no built-in, so the next
    /// launch fell back to dracula. `config.general.theme` is by definition
    /// the key, so it serves as both.
    pub(super) fn cycle_theme(&mut self) {
        let names = Theme::list_all_names();
        if names.is_empty() {
            return;
        }
        // Find current; if the configured value isn't a known key (a
        // hand-edited config naming a theme that no longer exists, say),
        // start from -1 so the first cycle lands on index 0 — and writes a
        // valid key on the way, so the state self-heals.
        let current_idx = names
            .iter()
            .position(|n| n == &self.config.general.theme)
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next_idx = ((current_idx + 1) as usize) % names.len();
        let next_name = &names[next_idx];

        match Theme::load(next_name) {
            Ok(theme) => {
                self.config.general.theme = next_name.clone();
                self.theme = theme;
                self.push_log(
                    LogLevel::Info,
                    format!("theme: {} ({}/{})", next_name, next_idx + 1, names.len()),
                );
                // Persist as a best-effort. A save failure here shouldn't
                // refuse the in-memory swap (the user can see the new theme
                // already), so we log and move on.
                if let Err(e) = self.config.save() {
                    self.push_log(
                        LogLevel::Warn,
                        format!("could not save theme preference: {e}"),
                    );
                }
            }
            Err(e) => {
                // Should be rare since list_all_names already probed parse-
                // ability, but a TOCTOU between probe and load is possible.
                self.push_log(
                    LogLevel::Error,
                    format!("theme {next_name} failed to load: {e}"),
                );
            }
        }
    }

    /// Snapshot of currently-running jobs, for the transfer strip and the
    /// cancel helpers above.
    ///
    /// Delegates to [`crate::transfer::TransferManager::active_jobs`], which
    /// filters while it holds the lock. Doing the filter here instead meant
    /// cloning every job the manager had ever tracked on every frame.
    pub fn active_jobs(&self) -> Vec<TransferJob> {
        self.transfer_manager
            .as_ref()
            .map(|m| m.active_jobs())
            .unwrap_or_default()
    }

    /// `(active, pending)` job counts, or `(0, 0)` when not connected —
    /// which is the case whenever quit is reached from the session selector.
    pub fn queue_counts(&self) -> (usize, usize) {
        self.transfer_manager
            .as_ref()
            .map(|m| m.queue_counts())
            .unwrap_or((0, 0))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::config::Config;
    use crate::paths;
    use crate::theme::Theme;
    use crate::transfer::{TransferEvent, TransferManager, TransferState};
    use crate::tui::app::{App, Pane, Screen};
    use crate::tui::state::PendingCancel;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn logged(a: &App, needle: &str) -> bool {
        a.log.iter().any(|l| l.message.contains(needle))
    }

    /// An app on the main screen with a transfer manager that runs up to
    /// three jobs at once, and the manager's events.
    fn connected() -> (App, UnboundedReceiver<TransferEvent>) {
        let mut a = App::new(Config::default(), Theme::load("dracula").unwrap());
        let (manager, events) = TransferManager::new(3);
        a.transfer_manager = Some(manager);
        a.screen = Screen::Main;
        a.active_pane = Pane::Transfers;
        (a, events)
    }

    fn manager(a: &App) -> &TransferManager {
        a.transfer_manager.as_ref().unwrap()
    }

    /// Queue a download of `/r/<name>`, in `batch` if given.
    fn queue(a: &App, name: &str, batch: Option<u64>) -> u64 {
        let (remote, local) = (format!("/r/{name}"), PathBuf::from(format!("/l/{name}")));
        match batch {
            Some(b) => manager(a).enqueue_download_batched(remote, local, b),
            None => manager(a).enqueue_download(remote, local),
        }
        .unwrap()
    }

    /// Start the next queued job, as the dispatcher would.
    fn start_one(a: &App) -> u64 {
        manager(a).take_next_pending().expect("a job is queued").id
    }

    // -- cancel ---------------------------------------------------------------

    #[test]
    fn cancelling_with_nothing_running_asks_nothing() {
        let (mut a, _events) = connected();
        queue(&a, "waiting.bin", None);

        a.request_cancel_selected_transfer();
        a.request_cancel_selected_batch();

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_cancel.is_none());
    }

    #[test]
    fn cancelling_without_a_connection_asks_nothing() {
        let mut a = App::new(Config::default(), Theme::load("dracula").unwrap());
        a.screen = Screen::Main;

        a.request_cancel_selected_transfer();
        a.request_cancel_selected_batch();

        assert_eq!(a.screen, Screen::Main);
    }

    #[test]
    fn c_asks_about_the_transfer_under_the_cursor() {
        let (mut a, _events) = connected();
        queue(&a, "one.bin", None);
        queue(&a, "two.bin", None);
        start_one(&a);
        let second = start_one(&a);
        a.transfer_cursor = 1;

        a.handle_key(press(KeyCode::Char('c')));

        assert_eq!(a.screen, Screen::ConfirmCancel);
        assert_eq!(a.previous_screen, Screen::Main);
        match a.pending_cancel.as_ref().unwrap() {
            PendingCancel::Single { id, name } => {
                assert_eq!((*id, name.as_str()), (second, "two.bin"));
            }
            other => panic!("expected a single cancel, got {other:?}"),
        }
    }

    /// Jobs finish under the cursor; one left past the end means the last.
    #[test]
    fn a_cursor_past_the_last_transfer_means_the_last() {
        let (mut a, _events) = connected();
        queue(&a, "one.bin", None);
        let only = start_one(&a);
        a.transfer_cursor = 7;

        a.request_cancel_selected_transfer();

        assert!(matches!(
            a.pending_cancel,
            Some(PendingCancel::Single { id, .. }) if id == only
        ));
    }

    #[test]
    fn confirming_cancels_the_transfer() {
        let (mut a, _events) = connected();
        queue(&a, "one.bin", None);
        let id = start_one(&a);
        a.request_cancel_selected_transfer();

        a.handle_confirm_cancel(press(KeyCode::Char('y')));

        assert!(matches!(
            manager(&a).job(id).unwrap().state,
            TransferState::Failed(_)
        ));
        assert!(manager(&a).active_jobs().is_empty());
        assert!(logged(&a, "cancelled: one.bin"));
        assert!(a.pending_cancel.is_none());
    }

    #[test]
    fn shift_c_asks_about_the_whole_batch_counting_what_is_queued() {
        let (mut a, _events) = connected();
        let batch = manager(&a).allocate_batch_id();
        for name in ["a.bin", "b.bin", "c.bin", "d.bin"] {
            queue(&a, name, Some(batch));
        }
        queue(&a, "unrelated.bin", None);
        start_one(&a);

        a.handle_key(press(KeyCode::Char('C')));

        assert_eq!(a.screen, Screen::ConfirmCancel);
        match a.pending_cancel.as_ref().unwrap() {
            PendingCancel::Batch {
                batch_id,
                active,
                pending,
                cursor_name,
            } => {
                assert_eq!(*batch_id, batch);
                assert_eq!((*active, *pending), (1, 3), "the other job isn't counted");
                assert_eq!(cursor_name, "a.bin");
            }
            other => panic!("expected a batch cancel, got {other:?}"),
        }
    }

    #[test]
    fn confirming_cancels_the_batch_and_leaves_other_jobs() {
        let (mut a, _events) = connected();
        let batch = manager(&a).allocate_batch_id();
        let in_batch: Vec<u64> = ["a.bin", "b.bin", "c.bin"]
            .iter()
            .map(|n| queue(&a, n, Some(batch)))
            .collect();
        let other = queue(&a, "unrelated.bin", None);
        start_one(&a);
        a.request_cancel_selected_batch();

        a.handle_confirm_cancel(press(KeyCode::Char('y')));

        for id in in_batch {
            assert!(
                matches!(manager(&a).job(id).unwrap().state, TransferState::Failed(_)),
                "job {id}"
            );
        }
        assert_eq!(
            manager(&a).job(other).unwrap().state,
            TransferState::Pending
        );
        assert!(logged(&a, "cancelled batch: 1 active + 2 queued"));
    }

    /// A job that isn't part of a batch still gets cancelled by the batch
    /// key, as itself.
    #[test]
    fn shift_c_on_a_lone_transfer_asks_about_just_that_one() {
        let (mut a, _events) = connected();
        queue(&a, "lone.bin", None);
        let id = start_one(&a);

        a.request_cancel_selected_batch();

        assert!(matches!(
            a.pending_cancel,
            Some(PendingCancel::Single { id: asked, .. }) if asked == id
        ));
    }

    /// `c` and `C` belong to the Transfers pane; elsewhere they are not
    /// cancels.
    #[test]
    fn the_cancel_keys_do_nothing_off_the_transfers_pane() {
        let (mut a, _events) = connected();
        queue(&a, "one.bin", None);
        start_one(&a);
        a.active_pane = Pane::Local;

        a.handle_key(press(KeyCode::Char('c')));
        a.handle_key(press(KeyCode::Char('C')));

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_cancel.is_none());
    }

    // -- pause ----------------------------------------------------------------

    #[test]
    fn p_pauses_and_p_again_resumes() {
        let (mut a, mut events) = connected();

        a.handle_key(press(KeyCode::Char('p')));
        assert!(manager(&a).is_paused());
        let ev = events.try_recv().expect("pausing is announced");
        assert!(matches!(ev, TransferEvent::Paused));
        a.handle_transfer_event(ev);
        assert!(logged(&a, "transfers paused"));

        a.handle_key(press(KeyCode::Char('p')));
        assert!(!manager(&a).is_paused());
        let ev = events.try_recv().expect("resuming is announced");
        assert!(matches!(ev, TransferEvent::Resumed));
        a.handle_transfer_event(ev);
        assert!(logged(&a, "transfers resumed"));
    }

    #[test]
    fn pausing_without_a_connection_says_so() {
        let mut a = App::new(Config::default(), Theme::load("dracula").unwrap());
        a.screen = Screen::Main;

        a.toggle_pause();

        assert!(logged(&a, "not connected"));
    }

    // -- queue counts ---------------------------------------------------------

    #[test]
    fn queue_counts_are_running_then_waiting() {
        let (a, _events) = connected();
        for name in ["a.bin", "b.bin", "c.bin"] {
            queue(&a, name, None);
        }
        start_one(&a);

        assert_eq!(a.queue_counts(), (1, 2));
    }

    #[test]
    fn queue_counts_are_zero_without_a_connection() {
        let a = App::new(Config::default(), Theme::load("dracula").unwrap());

        assert_eq!(a.queue_counts(), (0, 0));
    }

    // -- theme cycling --------------------------------------------------------

    /// The theme `config.ini` names, read back from disk.
    fn saved_theme() -> String {
        Config::load_from(&paths::config_file().unwrap())
            .unwrap()
            .general
            .theme
    }

    fn app_with_theme(name: &str) -> App {
        let mut config = Config::default();
        config.general.theme = name.to_string();
        App::new(config, Theme::load("dracula").unwrap())
    }

    #[test]
    fn t_moves_to_the_next_theme_and_saves_the_choice() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = app_with_theme(&names[0]);
        a.screen = Screen::Main;

        a.handle_key(press(KeyCode::Char('t')));

        assert_eq!(a.config.general.theme, names[1]);
        assert_eq!(a.theme.name, Theme::load(&names[1]).unwrap().name);
        assert_eq!(saved_theme(), names[1]);
        assert!(logged(
            &a,
            &format!("theme: {} (2/{})", names[1], names.len())
        ));
    }

    #[test]
    fn the_last_theme_is_followed_by_the_first() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = app_with_theme(names.last().unwrap());

        a.cycle_theme();

        assert_eq!(a.config.general.theme, names[0]);
    }

    /// A config naming a theme that no longer exists starts over from the
    /// first, and is put right on disk as it does.
    #[test]
    fn a_theme_that_no_longer_exists_cycles_to_the_first() {
        let _home = paths::test_home();
        let mut a = app_with_theme("long-gone");

        a.cycle_theme();

        assert_eq!(a.config.general.theme, Theme::list_all_names()[0]);
        assert_eq!(saved_theme(), a.config.general.theme);
    }

    /// A user theme is known by its file name. Its `[theme] name` is only
    /// a label: saved as the choice, it would name no theme at the next
    /// launch.
    #[test]
    fn a_user_theme_is_cycled_onto_and_saved_by_its_file_name() {
        let _home = paths::test_home();
        std::fs::write(
            paths::themes_dir().unwrap().join("aaa-mine.ini"),
            "[theme]\nname = My Cool Theme\n\n[colors]\n\
             bg=#000000\nfg=#ffffff\ndim=#888888\ncursor_bg=#111111\n\
             border_active=#ff00ff\nborder_inactive=#222222\naccent=#ff0000\n\
             directory=#00ffff\nimage=#ffff00\nselected=#00ff00\n\
             success=#00ff00\nwarning=#ffaa00\nerror=#ff0000\n",
        )
        .unwrap();
        let names = Theme::list_all_names();
        assert_eq!(names[0], "aaa-mine", "sorts first, among the built-ins");
        let mut a = app_with_theme(names.last().unwrap());

        a.cycle_theme();
        assert_eq!(a.theme.name, "My Cool Theme");
        assert_eq!(saved_theme(), "aaa-mine");

        a.cycle_theme();
        assert_eq!(
            a.config.general.theme, names[1],
            "and the cycle moves on from it"
        );
    }
}
