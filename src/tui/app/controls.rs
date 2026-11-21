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
//!
//! With the read-only helpers over the transfer queue the Transfers pane,
//! the cancels and the quit and disconnect questions share:
//! `transfer_row_count` / `transfer_rows` (the pane's rows, running then
//! queued) and `queue_counts`.

use crate::theme::Theme;
use crate::transfer::{TransferJob, TransferState};
use crate::tui::state::PendingCancel;

use super::{App, LogLevel, Screen, name_for_job};

impl App {
    /// The Transfers pane row under the cursor: a running job or a queued
    /// one, as the pane lists them.
    fn transfer_under_cursor(&self) -> Option<TransferJob> {
        let len = self.transfer_row_count();
        if len == 0 {
            return None;
        }
        let idx = self.transfer_cursor.min(len - 1);
        self.transfer_rows(idx, 1).into_iter().next()
    }

    /// Ask about cancelling the transfer under the cursor, running or
    /// queued. Only running ones could be picked before, so with nothing
    /// running a queue could not be touched, and a queued job outside a
    /// batch not at all.
    pub(super) fn request_cancel_selected_transfer(&mut self) {
        let Some(job) = self.transfer_under_cursor() else {
            return;
        };
        self.pending_cancel = Some(PendingCancel::Single {
            id: job.id,
            name: name_for_job(&job),
            queued: job.state == TransferState::Pending,
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
        let Some(cursor_job) = self.transfer_under_cursor() else {
            return;
        };

        let Some(batch_id) = cursor_job.batch_id else {
            // Not a batched job — fall through to the single-job modal so
            // the gesture still does something useful.
            self.request_cancel_selected_transfer();
            return;
        };

        // Count siblings in the batch, queued ones included, and those
        // waiting outside the queue for room: confirming cancels them too,
        // and leaving them out of the question understated it.
        let (active, queued) = manager.batch_counts(batch_id);
        let pending = queued + self.waiting_in_batch(batch_id);
        if active == 0 && pending == 0 {
            return;
        }

        self.pending_cancel = Some(PendingCancel::Batch {
            batch_id,
            active,
            pending,
            cursor_name: name_for_job(&cursor_job),
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

    /// Cycle to the next theme in `Theme::list_all_names`: every key,
    /// built-in or user, sorted together. The new theme is applied
    /// immediately and saved where the one on screen came from: the
    /// connected session's file, when that session names its own theme,
    /// and `config.ini` otherwise.
    ///
    /// It always went to `config.ini`. Over a session with its own theme
    /// that changed the theme everywhere else, and the session's came back
    /// on the next connect, so the choice never stuck where it was made.
    ///
    /// Both the position lookup and the persisted value use the theme's
    /// *key* (`Theme.key`) — the normalised name `Theme::load` accepts —
    /// rather than `Theme.name`, the display label from the file's optional
    /// `[theme] name` field. Conflating them broke the cycle (a display
    /// name never matched a listed key, so it jumped back to index 0) and
    /// the next launch (`theme = My Cool Theme` resolves to nothing).
    pub(super) fn cycle_theme(&mut self) {
        let names = Theme::list_all_names();
        if names.is_empty() {
            return;
        }
        // Start from the theme on screen — a session's own theme, if one is
        // showing, not the config's. If its key isn't in the list (a file
        // removed since it loaded, say), start from -1 so the first cycle
        // lands on index 0 — and writes a valid key on the way.
        let current_idx = names
            .iter()
            .position(|n| *n == self.theme.key)
            .map(|i| i as isize)
            .unwrap_or(-1);

        // A file that won't load is reported and stepped over; stopping on
        // it meant every later press retried it and nothing after it could
        // be reached. Built-ins always load, so this ends.
        for step in 1..=names.len() {
            let idx = ((current_idx + step as isize) as usize) % names.len();
            let next_name = &names[idx];
            match Theme::load(next_name) {
                Ok(theme) => {
                    self.theme = theme;
                    let next_name = next_name.clone();
                    let position = format!("({}/{})", idx + 1, names.len());
                    // Persist as a best-effort. A save failure here shouldn't
                    // refuse the in-memory swap (the user can see the new
                    // theme already), so we log and move on.
                    if let Some(session) = self.session_with_own_theme() {
                        self.push_log(
                            LogLevel::Info,
                            format!("theme: {next_name} {position}, for session {session}"),
                        );
                        if let Err(e) = self.save_session_theme(&next_name) {
                            self.push_log(
                                LogLevel::Warn,
                                format!("could not save the session's theme: {e}"),
                            );
                        }
                    } else {
                        self.config.general.theme = next_name.clone();
                        self.push_log(LogLevel::Info, format!("theme: {next_name} {position}"));
                        if let Err(e) = crate::config::save_setting("general", "theme", &next_name)
                        {
                            self.push_log(
                                LogLevel::Warn,
                                format!("could not save theme preference: {e}"),
                            );
                        }
                    }
                    return;
                }
                Err(e) => {
                    self.push_log(
                        LogLevel::Error,
                        format!("theme {next_name} failed to load, skipped: {e}"),
                    );
                }
            }
        }
    }

    /// The name of the connected session when it names its own theme and
    /// has a file to save one to.
    fn session_with_own_theme(&self) -> Option<String> {
        self.current_session
            .as_ref()
            .filter(|s| s.theme.is_some() && self.current_session_saved)
            .map(|s| s.name.clone())
    }

    /// Save `theme` as the connected session's own, in its file as it is
    /// on disk: only the theme changes, not where the panes have browsed
    /// to since connecting.
    fn save_session_theme(&mut self, theme: &str) -> crate::error::Result<()> {
        let Some(current) = self.current_session.as_mut() else {
            return Ok(());
        };
        current.theme = Some(theme.to_string());
        let mut on_disk = crate::session::Session::load_from(&current.path()?)?;
        on_disk.theme = Some(theme.to_string());
        on_disk.save()?;
        if let Some(listed) = self.sessions.iter_mut().find(|s| s.name == on_disk.name) {
            listed.theme = Some(theme.to_string());
        }
        Ok(())
    }

    /// How many rows the Transfers pane lists: running jobs, then queued.
    pub fn transfer_row_count(&self) -> usize {
        self.transfer_manager
            .as_ref()
            .map_or(0, |m| m.listed_count())
    }

    /// Rows `start..start + len` of the Transfers pane, as
    /// [`crate::transfer::TransferManager::listed_jobs`] gives them: only
    /// the window is cloned.
    pub fn transfer_rows(&self, start: usize, len: usize) -> Vec<TransferJob> {
        self.transfer_manager
            .as_ref()
            .map(|m| m.listed_jobs(start, len))
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
    fn cancelling_with_nothing_listed_asks_nothing() {
        let (mut a, _events) = connected();

        a.request_cancel_selected_transfer();
        a.request_cancel_selected_batch();

        assert_eq!(a.screen, Screen::Main);
        assert!(a.pending_cancel.is_none());
    }

    /// Only running jobs could be picked, so with nothing running a queued
    /// job couldn't be cancelled, and one outside a batch never could.
    #[test]
    fn a_queued_transfer_can_be_cancelled_with_nothing_running() {
        let (mut a, _events) = connected();
        queue(&a, "waiting.bin", None);

        a.handle_key(press(KeyCode::Char('c')));
        match a.pending_cancel.as_ref().unwrap() {
            PendingCancel::Single { name, queued, .. } => {
                assert_eq!(name, "waiting.bin");
                assert!(*queued, "the question knows it hasn't started");
            }
            other => panic!("expected a single cancel, got {other:?}"),
        }
        a.handle_confirm_cancel(press(KeyCode::Char('y')));

        assert_eq!(a.transfer_row_count(), 0);
        assert!(logged(&a, "cancelled: waiting.bin"), "{:?}", a.log);
    }

    /// The cursor reaches past the running jobs onto the queued ones.
    #[test]
    fn the_cursor_reaches_the_queued_rows_below_the_running() {
        let (mut a, _events) = connected();
        queue(&a, "running.bin", None);
        queue(&a, "queued.bin", None);
        start_one(&a);
        a.transfer_cursor = 1;

        a.handle_key(press(KeyCode::Char('c')));

        match a.pending_cancel.as_ref().unwrap() {
            PendingCancel::Single { name, queued, .. } => {
                assert_eq!((name.as_str(), *queued), ("queued.bin", true));
            }
            other => panic!("expected a single cancel, got {other:?}"),
        }
    }

    #[test]
    fn a_queued_batch_can_be_cancelled_with_nothing_running() {
        let (mut a, _events) = connected();
        let b = manager(&a).allocate_batch_id();
        queue(&a, "one.bin", Some(b));
        queue(&a, "two.bin", Some(b));

        a.handle_key(press(KeyCode::Char('C')));
        assert!(matches!(
            a.pending_cancel,
            Some(PendingCancel::Batch {
                active: 0,
                pending: 2,
                ..
            })
        ));
        a.handle_confirm_cancel(press(KeyCode::Char('y')));

        assert_eq!(a.transfer_row_count(), 0);
        assert!(
            logged(&a, "cancelled batch: 0 active + 2 queued"),
            "{:?}",
            a.log
        );
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
            PendingCancel::Single { id, name, .. } => {
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

    /// An app showing `name`, as startup would: falling back to dracula
    /// when it doesn't load.
    fn app_with_theme(name: &str) -> App {
        let mut config = Config::default();
        config.general.theme = name.to_string();
        let (theme, _) = Theme::load_or_fallback(name);
        App::new(config, theme)
    }

    /// A saved session naming its own theme, connected and showing it.
    fn connected_with_session_theme(names: &[String]) -> App {
        let mut s = crate::session::Session::from_url("sftp://me@h.example").unwrap();
        s.name = "work".into();
        s.theme = Some(names[0].clone());
        s.save().unwrap();
        let mut a = app_with_theme(&names[1]);
        a.screen = Screen::Main;
        a.theme = Theme::load(&names[0]).unwrap();
        a.current_session = Some(s);
        a.current_session_saved = true;
        a
    }

    /// Over a session with its own theme, `t` went to `config.ini`: the
    /// theme changed everywhere else, and the session's came back on the
    /// next connect.
    #[test]
    fn t_over_a_session_theme_saves_it_to_the_session() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = connected_with_session_theme(&names);
        // Browsed since connecting: not something `t` should save.
        a.remote.path = "/elsewhere".into();

        a.handle_key(press(KeyCode::Char('t')));

        let on_disk = crate::session::Session::list_all()
            .unwrap()
            .into_iter()
            .find(|s| s.name == "work")
            .unwrap();
        assert_eq!(on_disk.theme.as_deref(), Some(names[1].as_str()));
        assert_eq!(on_disk.remote_dir, "/", "only the theme is saved");
        assert_eq!(a.config.general.theme, names[1], "the default is untouched");
        assert!(
            !paths::config_file().unwrap().exists(),
            "config.ini isn't written"
        );
        assert!(logged(&a, "for session work"), "{:?}", a.log);
    }

    /// A session without its own theme leaves `t` to the default, as on
    /// the session list.
    #[test]
    fn t_over_a_session_without_its_own_theme_saves_the_default() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = app_with_theme(&names[0]);
        a.screen = Screen::Main;
        let mut s = crate::session::Session::from_url("sftp://me@h.example").unwrap();
        s.name = "work".into();
        s.save().unwrap();
        a.current_session = Some(s);
        a.current_session_saved = true;

        a.handle_key(press(KeyCode::Char('t')));

        assert_eq!(saved_theme(), names[1]);
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

    /// A config naming a theme that no longer exists shows dracula, so `t`
    /// moves on from dracula — and writes a valid key over the stale one.
    #[test]
    fn a_theme_that_no_longer_exists_cycles_on_from_the_fallback() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = app_with_theme("long-gone");

        a.cycle_theme();

        let i = names.iter().position(|n| n == "dracula").unwrap();
        assert_eq!(a.config.general.theme, names[(i + 1) % names.len()]);
        assert_eq!(saved_theme(), a.config.general.theme);
    }

    /// A theme on screen whose key has left the list (its file deleted
    /// since) starts the cycle over from the first.
    #[test]
    fn a_theme_no_longer_listed_cycles_to_the_first() {
        let _home = paths::test_home();
        let mut a = app_with_theme("dracula");
        a.theme.key = "deleted-since".into();

        a.cycle_theme();

        assert_eq!(a.config.general.theme, Theme::list_all_names()[0]);
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

    /// A theme file that won't load was retried on every press, so the
    /// cycle stuck on it and nothing after it could be reached.
    #[test]
    fn a_broken_theme_is_skipped_not_stuck_on() {
        let _home = paths::test_home();
        std::fs::write(
            paths::themes_dir().unwrap().join("aaa-broken.ini"),
            "[colors]\nbg=nope\n",
        )
        .unwrap();
        let names = Theme::list_all_names();
        assert_eq!(names[0], "aaa-broken");
        let mut a = app_with_theme(names.last().unwrap());

        a.cycle_theme();
        assert_eq!(
            a.config.general.theme, names[1],
            "stepped over the broken one"
        );
        assert!(logged(&a, "theme aaa-broken failed to load, skipped"));

        a.cycle_theme();
        assert_eq!(a.config.general.theme, names[2], "and keeps going");
    }

    /// With a session's theme on screen, `t` moves on from that one, not
    /// from the config's.
    #[test]
    fn t_cycles_from_the_theme_on_screen() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = app_with_theme("dracula");
        a.theme = Theme::load("nord").unwrap();

        a.cycle_theme();

        let after_nord =
            &names[(names.iter().position(|n| n == "nord").unwrap() + 1) % names.len()];
        assert_eq!(&a.config.general.theme, after_nord);
    }

    /// `Tokyo Night` loads tokyo-night, so the cycle has to know it's there
    /// rather than starting over from the first theme.
    #[test]
    fn a_loosely_spelled_config_theme_cycles_on_from_itself() {
        let _home = paths::test_home();
        let names = Theme::list_all_names();
        let mut a = app_with_theme("Tokyo Night");

        a.cycle_theme();

        let i = names.iter().position(|n| n == "tokyo-night").unwrap();
        assert_eq!(a.config.general.theme, names[(i + 1) % names.len()]);
    }
}
