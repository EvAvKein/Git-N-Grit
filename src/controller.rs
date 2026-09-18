//! The screen's own state and the keys: which mode is showing, the selection,
//! scrolling — and the [`Model`] each of those drives.
//!
//! Nothing here touches a repo's filesystem, a scan, a watch or a thread
//! directly; every such thing is a call to the model it holds.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};

use anyhow::Result;
use ratatui::crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind};
use uuid::Uuid;

use crate::event::AppEvent;
use crate::model::{AddOutcome, InactiveReason, Model};
use crate::scan::Resolved;
use crate::state::{LoadError, RepoState, Storage};

/// A line of the left pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Add,
    Repo(Uuid),
    /// A state file that would not load, labelled by its own name.
    BadFile(PathBuf),
}

/// One line of the picker: an existing row this path could be, or a fresh one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSelection {
    Tracked(Uuid),
    New,
}

/// What the screen is asking for, and what the keys mean while it does.
#[derive(Debug)]
pub enum UiState {
    List,
    AddPrompt {
        input: String,
        error: Option<String>,
        checking: bool,
    },
    Picker {
        resolved: Resolved,
        lines: Vec<RepoSelection>,
        selected_line: usize,
    },
    ConfirmRemove {
        target: Row,
        /// Remove, rather than the Cancel the confirm starts on.
        confirmed: bool,
    },
}

pub struct Controller {
    model: Model,

    selected: usize,
    /// Events back from the newest, which is what `PgUp`/`PgDn` move.
    scroll: usize,
    page: usize,
    mode: UiState,
    /// The add prompt's generation. An answer carrying any other is discarded.
    seq: u64,

    quit: bool,
}

impl Controller {
    pub fn new(storage: Storage, outbox: Sender<AppEvent>) -> Result<Controller> {
        Ok(Controller {
            model: Model::new(storage, outbox)?,
            selected: 0,
            scroll: 0,
            page: 10, // overwritten once the right pane's renderer reports its real height
            mode: UiState::List,
            seq: 0,
            quit: false,
        })
    }

    /// Draw, wait, handle, repeat. Drawing is the caller's, so the terminal
    /// stays out of here; `&mut Self` is what lets the renderer report its page
    /// size back through [`set_page`](Controller::set_page).
    pub fn run(
        &mut self,
        inbox: &Receiver<AppEvent>,
        mut draw: impl FnMut(&mut Self) -> Result<()>,
    ) -> Result<()> {
        while !self.quit {
            draw(self)?;
            match self.wait(inbox) {
                Ok(event) => self.handle(event)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.check_wedged();
        }
        Ok(())
    }

    fn wait(&self, inbox: &Receiver<AppEvent>) -> Result<AppEvent, RecvTimeoutError> {
        if self.model.may_wedge() {
            inbox.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            inbox.recv_timeout(crate::model::WEDGE_LIMIT)
        }
    }

    /// The `Result` is the loop's, for what a future arm has to hand upwards.
    /// Nothing a key does reaches it: a removal that fails takes its own row.
    pub fn handle(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Input(CtEvent::Key(key)) => self.on_key(key),
            AppEvent::Input(_) => {}
            AppEvent::Fs(paths) => {
                if self.model.on_fs(paths) {
                    self.quit = true;
                }
            }
            AppEvent::ScanDone {
                incarnation,
                id,
                result,
            } => self.model.on_scan_done(incarnation, id, result),
            AppEvent::Returned(ids) => {
                for id in ids {
                    self.model.queue_scan(id);
                }
            }
            AppEvent::Resolved { seq, result } => self.on_resolved(seq, result),
        }
        Ok(())
    }

    // ---- rows -------------------------------------------------------------

    pub fn row_count(&self) -> usize {
        1 + self.model.row_count()
    }

    pub fn row(&self, index: usize) -> Option<Row> {
        let Some(i) = index.checked_sub(1) else {
            return Some(Row::Add);
        };
        match self.model.repos().get(i) {
            Some(state) => Some(Row::Repo(state.id)),
            None => self
                .model
                .bad_files()
                .get(i - self.model.repos().len())
                .map(|e| Row::BadFile(e.path.clone())),
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_repo(&self) -> Option<&RepoState> {
        match self.row(self.selected)? {
            Row::Repo(id) => self.model.state(id),
            _ => None,
        }
    }

    pub fn mode(&self) -> &UiState {
        &self.mode
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn storage(&self) -> &Storage {
        self.model.storage()
    }

    /// Why a repo is dimmed, for the row and the right pane's `!` message.
    pub fn inactive_reason(&self, id: Uuid) -> Option<InactiveReason> {
        self.model.inactive_reason(id)
    }

    /// A repo's state by id, for the picker's candidates — not just the
    /// selected row.
    pub fn state(&self, id: Uuid) -> Option<&RepoState> {
        self.model.state(id)
    }

    /// Why a state file didn't load, for the right pane on a selected
    /// [`Row::BadFile`].
    pub fn bad_file(&self, path: &Path) -> Option<&LoadError> {
        self.model.bad_file(path)
    }

    /// How many event rows the right pane fits, for `PgUp`/`PgDn` to move by.
    pub fn set_page(&mut self, events: usize) {
        self.page = events.max(1);
    }

    // ---- keys -------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        // A release would otherwise act a second time on every key.
        if key.kind == KeyEventKind::Release {
            return;
        }
        match self.mode {
            UiState::List => self.key_list(key.code),
            UiState::AddPrompt { .. } => self.key_prompt(key.code),
            UiState::Picker { .. } => self.key_picker(key.code),
            UiState::ConfirmRemove { .. } => self.key_confirm(key.code),
        }
    }

    fn key_list(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.scroll_pages(-1),
            KeyCode::PageDown => self.scroll_pages(1),
            KeyCode::Home => self.scroll = 0,
            KeyCode::Enter if self.row(self.selected) == Some(Row::Add) => self.open_prompt(),
            KeyCode::Char('a') => self.open_prompt(),
            KeyCode::Char('d') => self.open_confirm(),
            KeyCode::Char('r') => self.model.rescan_all(),
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    fn key_prompt(&mut self, code: KeyCode) {
        match code {
            // Bumping the generation is what discards an answer still in flight.
            KeyCode::Esc => {
                self.seq += 1;
                self.mode = UiState::List;
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Backspace => {
                if let UiState::AddPrompt { input, .. } = &mut self.mode {
                    input.pop();
                }
            }
            KeyCode::Char(c) => {
                if let UiState::AddPrompt { input, .. } = &mut self.mode {
                    input.push(c);
                }
            }
            _ => {}
        }
    }

    fn key_picker(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.mode = UiState::List,
            KeyCode::Enter => self.take_pick(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let UiState::Picker { selected_line, .. } = &mut self.mode {
                    *selected_line = selected_line.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let UiState::Picker {
                    lines,
                    selected_line,
                    ..
                } = &mut self.mode
                {
                    // Saturating: empty `lines` would otherwise underflow.
                    *selected_line = (*selected_line + 1).min(lines.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }

    fn key_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.mode = UiState::List,
            KeyCode::Enter => self.take_confirm(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let UiState::ConfirmRemove { confirmed, .. } = &mut self.mode {
                    *confirmed = false;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let UiState::ConfirmRemove { confirmed, .. } = &mut self.mode {
                    *confirmed = true;
                }
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, by: isize) {
        let last = self.row_count().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
        // The pane now shows a different repo.
        self.scroll = 0;
    }

    fn scroll_pages(&mut self, by: isize) {
        let events = self.selected_repo().map_or(0, |s| s.events.len());
        let furthest = events.saturating_sub(self.page);
        self.scroll = self
            .scroll
            .saturating_add_signed(by.saturating_mul(self.page as isize))
            .min(furthest);
    }

    fn open_prompt(&mut self) {
        self.seq += 1;
        self.mode = UiState::AddPrompt {
            input: String::new(),
            error: None,
            checking: false,
        };
    }

    fn open_confirm(&mut self) {
        let target = match self.row(self.selected) {
            // Every repo sits behind this directory, so deleting it is not the
            // answer to it not reading.
            Some(Row::BadFile(path)) if path == self.model.storage().repos_dir() => return,
            Some(target @ (Row::Repo(_) | Row::BadFile(_))) => target,
            _ => return,
        };
        self.mode = UiState::ConfirmRemove {
            target,
            confirmed: false,
        };
    }

    fn submit_prompt(&mut self) {
        let path = {
            let UiState::AddPrompt {
                input,
                error,
                checking,
            } = &mut self.mode
            else {
                return;
            };
            let path = PathBuf::from(input.trim());
            if path.as_os_str().is_empty() {
                return;
            }
            *error = None;
            *checking = true;
            path
        };
        // Nothing can free the thread already inside a call on this one, so
        // asking again would only cost another.
        if self.model.is_wedged(&path) {
            self.tell_prompt(&path);
            return;
        }
        self.seq += 1;
        let states = self.model.repos().to_vec();
        self.model.resolve(self.seq, path, states);
    }

    /// Report a path that stopped answering to the add prompt. A typed path
    /// belongs to no row, so this is the only place it can show.
    fn tell_prompt(&mut self, path: &Path) {
        if let UiState::AddPrompt {
            error, checking, ..
        } = &mut self.mode
        {
            *error = Some(format!("{} is not responding", path.display()));
            *checking = false;
        }
        // An answer that arrives after all must not reopen what this closed.
        self.seq += 1;
    }

    fn take_pick(&mut self) {
        match std::mem::replace(&mut self.mode, UiState::List) {
            UiState::Picker {
                resolved,
                lines,
                selected_line,
            } => match lines.get(selected_line) {
                Some(RepoSelection::Tracked(id)) => {
                    let id = self.model.register_into(*id, &resolved);
                    self.finish_add(id);
                }
                Some(RepoSelection::New) => {
                    let id = self.model.register_new(&resolved);
                    self.finish_add(id);
                }
                None => self.mode = UiState::List,
            },
            other => self.mode = other,
        }
    }

    fn take_confirm(&mut self) {
        match std::mem::replace(&mut self.mode, UiState::List) {
            UiState::ConfirmRemove {
                target,
                confirmed: true,
            } => self.remove(&target),
            UiState::ConfirmRemove { .. } => {}
            other => self.mode = other,
        }
    }

    /// A file that will not delete takes its own row rather than the loop: the
    /// row is the only place the failure can show.
    fn remove(&mut self, target: &Row) {
        match target {
            Row::Repo(id) => self.model.remove_repo(*id),
            Row::BadFile(path) => self.model.remove_bad_file(path),
            Row::Add => {}
        }
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    // ---- registering ------------------------------------------------------

    fn on_resolved(&mut self, seq: u64, result: Result<Resolved>) {
        self.model.resolved();
        if seq != self.seq {
            return;
        }
        match self.model.resolve_add(result) {
            AddOutcome::Registered(id) => self.finish_add(id),
            AddOutcome::Pick {
                resolved,
                candidates,
                offer_new,
            } => {
                let mut lines: Vec<RepoSelection> =
                    candidates.into_iter().map(RepoSelection::Tracked).collect();
                if offer_new {
                    lines.push(RepoSelection::New);
                }
                self.mode = UiState::Picker {
                    resolved,
                    lines,
                    selected_line: 0,
                };
            }
            AddOutcome::Failed(message) => {
                if let UiState::AddPrompt {
                    error, checking, ..
                } = &mut self.mode
                {
                    *error = Some(message);
                    *checking = false;
                }
            }
        }
    }

    fn finish_add(&mut self, id: Uuid) {
        self.mode = UiState::List;
        if let Some(i) = self.model.repos().iter().position(|s| s.id == id) {
            self.selected = i + 1;
            self.scroll = 0;
        }
        self.model.queue_scan(id);
    }

    // ---- wedged threads ---------------------------------------------------

    fn check_wedged(&mut self) {
        for path in self.model.check_wedged() {
            self.tell_prompt(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    use crate::git::RepoIdentity;
    use crate::scan::ScanFindings;
    use crate::state::{LoadError, Match, Ownership};

    /// A controller over a throwaway storage root, removed when the test ends.
    struct Harness {
        root: PathBuf,
        controller: Controller,
    }

    impl Harness {
        /// Registered but never scanned, which is the state at startup.
        fn new(count: usize) -> Harness {
            let root = std::env::temp_dir().join(format!("gng-controller-{}", Uuid::new_v4()));
            let storage = Storage::new(&root);
            for i in 0..count {
                storage
                    .save(&repo(&root, &format!("r{i}")))
                    .expect("saving");
            }
            let (outbox, _inbox) = mpsc::channel();
            let controller = Controller::new(storage, outbox).expect("building the controller");
            Harness { root, controller }
        }

        /// Every repo's first findings landed and found nothing: the steady state.
        fn settled(count: usize) -> Harness {
            let mut harness = Harness::new(count);
            for id in harness.ids() {
                harness.deliver(id, ScanFindings::default());
            }
            harness
        }

        fn ids(&self) -> Vec<Uuid> {
            self.controller.model.repos().iter().map(|s| s.id).collect()
        }

        fn id(&self, index: usize) -> Uuid {
            self.ids()[index]
        }

        fn deliver(&mut self, id: Uuid, findings: ScanFindings) {
            self.controller.model.on_scan_done(0, id, Ok(findings));
        }

        fn type_path(&mut self, path: &str) {
            for c in path.chars() {
                self.press(KeyCode::Char(c));
            }
        }

        fn press(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE);
            self.controller
                .handle(AppEvent::Input(CtEvent::Key(key)))
                .expect("handling a key");
        }

        /// The worker's answer to the resolve the prompt just posted, for a
        /// repo whose files are at `source_dir`.
        fn answer(&mut self, matched: Match, source_dir: PathBuf) {
            let seq = self.controller.seq;
            self.controller
                .handle(AppEvent::Resolved {
                    seq,
                    result: Ok(Resolved {
                        identity: RepoIdentity {
                            root_commits: vec!["added-root".into()],
                            git_common_dir: source_dir.join(".git"),
                            head_subdir: PathBuf::new(),
                        },
                        source_dir,
                        matched,
                    }),
                })
                .expect("handling an answer");
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn repo(root: &Path, name: &str) -> RepoState {
        let identity = RepoIdentity {
            root_commits: vec![format!("{name}-root")],
            git_common_dir: root.join(name).join(".git"),
            head_subdir: PathBuf::new(),
        };
        RepoState::new(&identity, &root.join(name))
    }

    /// `r` is the manual refresh, so it is also the way back from a path that
    /// stopped answering and has since recovered.
    #[test]
    fn r_tries_a_path_that_stopped_answering_again() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let common_dir = harness
            .controller
            .model
            .state(id)
            .expect("the repo")
            .git_common_dir
            .clone();
        harness.controller.model.mark_not_responding(&common_dir);

        harness.press(KeyCode::Char('r'));

        assert!(harness.controller.model.is_scanning(id));
        assert_eq!(harness.controller.model.inactive_reason(id), None);
    }

    #[test]
    fn an_answer_to_a_prompt_that_has_moved_on_is_discarded() {
        let mut harness = Harness::settled(0);
        harness.press(KeyCode::Char('a'));
        harness.type_path("/late");
        harness.press(KeyCode::Enter);
        let answered = harness.controller.seq;
        assert_eq!(harness.controller.model.unresolved_paths(), 1);
        harness.press(KeyCode::Esc);

        harness
            .controller
            .handle(AppEvent::Resolved {
                seq: answered,
                result: Ok(Resolved {
                    source_dir: harness.root.join("late"),
                    identity: RepoIdentity {
                        root_commits: vec!["late-root".into()],
                        git_common_dir: harness.root.join("late/.git"),
                        head_subdir: PathBuf::new(),
                    },
                    matched: Match::New,
                }),
            })
            .expect("handling a stale answer");

        assert_eq!(harness.controller.row_count(), 1);
    }

    #[test]
    fn in_the_prompt_the_command_keys_type_themselves() {
        let mut harness = Harness::settled(0);
        harness.press(KeyCode::Char('a'));

        for key in "adrq".chars() {
            harness.press(KeyCode::Char(key));
        }

        let UiState::AddPrompt { input, .. } = harness.controller.mode() else {
            panic!("the prompt closed");
        };
        assert_eq!(input, "adrq");
        assert!(!harness.controller.quit);
    }

    #[test]
    fn removing_a_repo_deletes_its_state_file_and_leaves_the_others_alone() {
        let mut harness = Harness::settled(2);
        let (doomed, kept) = (harness.id(0), harness.id(1));
        let untouched = fs::read(harness.root.join("repos").join(format!("{kept}.json")))
            .expect("reading the kept state file");

        harness.press(KeyCode::Down); // off `+ Add repo…`, onto the first repo
        harness.press(KeyCode::Char('d'));
        harness.press(KeyCode::Down); // Cancel → Remove
        harness.press(KeyCode::Enter);

        assert!(harness.controller.model.state(doomed).is_none());
        assert!(
            !harness
                .root
                .join("repos")
                .join(format!("{doomed}.json"))
                .exists()
        );
        assert_eq!(
            fs::read(harness.root.join("repos").join(format!("{kept}.json")))
                .expect("re-reading the kept state file"),
            untouched
        );
        assert!(harness.controller.model.watches().is_empty());
    }

    #[test]
    fn cancel_is_where_the_confirm_starts() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);

        harness.press(KeyCode::Down);
        harness.press(KeyCode::Char('d'));
        harness.press(KeyCode::Enter);

        assert!(harness.controller.model.state(id).is_some());
        assert!(matches!(harness.controller.mode(), UiState::List));
    }

    #[test]
    fn removing_a_state_file_that_would_not_load_deletes_that_file() {
        let mut harness = Harness::settled(0);
        let stray = harness.root.join("repos").join("not-a-uuid.json");
        fs::create_dir_all(harness.root.join("repos")).expect("creating repos/");
        fs::write(&stray, b"{}").expect("writing the stray file");
        harness.controller.model.inject_bad_file(LoadError {
            path: stray.clone(),
            message: "filename is not a UUID".into(),
        });

        harness.press(KeyCode::Down);
        harness.press(KeyCode::Char('d'));
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);

        assert!(!stray.exists());
        assert_eq!(harness.controller.row_count(), 1);
    }

    #[test]
    fn the_repos_directory_row_offers_no_removal() {
        let mut harness = Harness::settled(0);
        harness.controller.model.inject_bad_file(LoadError {
            path: harness.controller.storage().repos_dir(),
            message: "Permission denied (os error 13)".into(),
        });

        harness.press(KeyCode::Down);
        harness.press(KeyCode::Char('d'));

        assert!(matches!(harness.controller.mode(), UiState::List));
    }

    #[test]
    fn a_state_file_that_will_not_delete_takes_its_row_rather_than_the_app() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        // A directory where the file was: `remove_file` refuses that whoever
        // is running, where a read-only parent would not.
        let path = harness.root.join("repos").join(format!("{id}.json"));
        fs::remove_file(&path).expect("removing the state file");
        fs::create_dir(&path).expect("putting a directory in its place");

        harness.press(KeyCode::Down);
        harness.press(KeyCode::Char('d'));
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);

        assert!(harness.controller.model.state(id).is_some());
        assert!(
            harness
                .controller
                .model
                .inactive_reason(id)
                .is_some_and(|m| m.to_string().starts_with("could not remove"))
        );
    }

    #[test]
    fn a_stray_file_that_will_not_delete_says_so_on_its_own_row() {
        let mut harness = Harness::settled(0);
        let stray = harness.root.join("repos").join("not-a-uuid.json");
        fs::create_dir_all(&stray).expect("creating the stray entry");
        harness.controller.model.inject_bad_file(LoadError {
            path: stray.clone(),
            message: "filename is not a UUID".into(),
        });

        harness.press(KeyCode::Down);
        harness.press(KeyCode::Char('d'));
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);

        assert_eq!(harness.controller.row_count(), 2);
        let bad = harness.controller.model.bad_file(&stray).expect("the row");
        assert!(bad.message.starts_with("could not remove"));
    }

    #[test]
    fn a_path_that_stopped_answering_is_refused_at_the_prompt() {
        let mut harness = Harness::settled(0);
        harness
            .controller
            .model
            .mark_not_responding(&PathBuf::from("/mnt/gone"));

        harness.press(KeyCode::Char('a'));
        harness.type_path("/mnt/gone");
        harness.press(KeyCode::Enter);

        let UiState::AddPrompt {
            error, checking, ..
        } = harness.controller.mode()
        else {
            panic!("the prompt closed");
        };
        assert!(
            error
                .as_deref()
                .is_some_and(|e| e.contains("not responding"))
        );
        assert!(!*checking);
        // Nothing was posted, so nothing is waiting to come back.
        assert_eq!(harness.controller.model.unresolved_paths(), 0);
    }

    /// A second state on a live registered store would score it alongside the
    /// first, so "new repo" is not on offer where every candidate is on it.
    #[test]
    fn the_picker_withholds_new_repo_over_a_live_registered_store() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.press(KeyCode::Char('a'));
        harness.type_path("/repo");
        harness.press(KeyCode::Enter);

        // The store it resolved to is the one that row already names.
        harness.answer(Match::Selection(vec![id]), harness.root.join("r0"));
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);

        // Adopted into the one candidate rather than minted beside it.
        assert_eq!(harness.controller.row_count(), 2);
    }

    #[test]
    fn the_picker_offers_new_repo_where_the_candidates_have_moved_away() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.press(KeyCode::Char('a'));
        harness.type_path("/repo");
        harness.press(KeyCode::Enter);

        // A store no row names, the candidate's own having moved away.
        harness.answer(Match::Selection(vec![id]), harness.root.join("elsewhere"));
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);

        assert_eq!(harness.controller.row_count(), 3);
    }

    /// No candidate and the store already named by the one repo present:
    /// `lines` comes back empty, and `Down` must not panic on it.
    #[test]
    fn an_empty_picker_does_not_panic_on_down() {
        let mut harness = Harness::settled(1);
        harness.press(KeyCode::Char('a'));
        harness.type_path("/repo");
        harness.press(KeyCode::Enter);

        harness.answer(Match::Selection(vec![]), harness.root.join("r0"));
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);

        assert_eq!(harness.controller.row_count(), 2);
        assert!(matches!(harness.controller.mode(), UiState::List));
    }

    #[test]
    fn a_takeover_file_appearing_runs_the_quit_path() {
        let mut harness = Harness::settled(0);
        let takeover = harness.controller.storage().takeover_path();
        fs::write(&takeover, b"12345").expect("writing .takeover");

        harness
            .controller
            .handle(AppEvent::Fs(vec![takeover]))
            .expect("handling the handover");

        assert!(harness.controller.quit);
    }

    #[test]
    fn moving_the_selection_returns_the_pane_to_newest() {
        let mut harness = Harness::settled(2);
        let id = harness.id(0);
        harness.deliver(
            id,
            ScanFindings {
                ownership: Some(Ownership::Ours),
                events: vec![crate::state::Event {
                    sha: "a0f0365".into(),
                    timestamp: 1756100000,
                    subject: "feat: parse reflog".into(),
                    added: 1,
                    deleted: 0,
                    xp: 1,
                    rewrite: false,
                }],
                new_awarded: vec![crate::scan::Awarded {
                    sha: "a0f0365".into(),
                    xp: 1,
                }],
                ..ScanFindings::default()
            },
        );
        harness.press(KeyCode::Down);
        harness.controller.set_page(1);
        harness.press(KeyCode::PageDown);

        harness.press(KeyCode::Down);

        assert_eq!(harness.controller.scroll(), 0);
    }
}
