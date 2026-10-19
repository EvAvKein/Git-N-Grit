//! The domain state: every repo in memory, storage, and the threads that keep
//! them live.
//!
//! Nothing here knows what is on screen. Liveness, `git` and every `stat` are
//! the [worker](crate::model::scan)'s and the
//! [ticker](crate::model::scan::stale)'s answers, arriving as [`AppEvent`]s;
//! this decides what they mean for a repo.

pub mod event;
pub mod git;
pub mod scan;
mod scanner;
pub mod storage;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventHandler, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use uuid::Uuid;

use crate::model::event::AppEvent;
use crate::model::git::RepoIdentity;
use crate::model::scan::stale::{Probe, Ticker};
use crate::model::scan::{Resolved, ScanFindings};
use crate::model::scanner::Scanner;
use crate::model::storage::identity::{Match, share_a_root_commit};
use crate::model::storage::repo::{Duplicate, Ownership, RepoState};
use crate::model::storage::{LoadError, Storage};

/// A filesystem call outstanding this long is one nothing can free.
pub const WEDGE_LIMIT: Duration = Duration::from_secs(10);

/// How often a store that has gone missing is stat-ed for its return.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// One directory `notify` watches, and why.
pub enum WatchEntry {
    /// Reacts to a `HEAD` write under this directory — the reflog watch every
    /// live worktree has always had.
    Reflog(HashSet<Uuid>),
    /// Reacts to a change of a tracked child by name — a live worktree's own
    /// directory, watched from its parent so a rename of it is seen at all.
    /// Keyed by the child's current basename: two worktrees can't share one
    /// name under the same parent at once, so this is a safe, direct lookup.
    Parent(HashMap<OsString, Uuid>),
}

/// Why a repo is earning nothing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InactiveReason {
    NotResponding,
    Shadowed,
    Undecided,
    Moved,
    Foreign,
    Rerooted,
    NoEmail,
    Other(String),
}

impl fmt::Display for InactiveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InactiveReason::NotResponding => {
                f.write_str("this repo's filesystem is not responding")
            }
            InactiveReason::Shadowed => {
                f.write_str("this repo appears twice; the other one is earning")
            }
            InactiveReason::Undecided => {
                f.write_str("this repo appears twice; neither is earning until you delete one")
            }
            InactiveReason::Moved => f.write_str("this repo has moved or been deleted"),
            InactiveReason::Foreign => f.write_str("a different repo is at this path now"),
            InactiveReason::Rerooted => {
                f.write_str("this repo's history was rewritten since you added it")
            }
            InactiveReason::NoEmail => {
                f.write_str("no user.email is set, so no commit can be matched")
            }
            InactiveReason::Other(e) => f.write_str(e),
        }
    }
}

/// What resolving an add produced.
pub enum AddOutcome {
    /// Adopted into an existing state file or minted a new one.
    Registered(Uuid),
    /// Ambiguous: the controller builds a picker from the candidates, adding
    /// "new repo" only where `offer_new` says a fresh state file would not
    /// double-count an already-registered store.
    Pick {
        resolved: Resolved,
        candidates: Vec<Uuid>,
        offer_new: bool,
    },
    Failed(String),
}

pub struct Model {
    storage: Storage,
    outbox: Sender<AppEvent>,

    repos: Vec<RepoState>,
    bad_files: Vec<LoadError>,

    watcher: RecommendedWatcher,
    watches: HashMap<PathBuf, WatchEntry>,

    /// Paths no thread may touch again, because one is still inside a call on
    /// this one and always will be.
    wedged: HashSet<PathBuf>,

    scanner: Scanner,
    ticker: Ticker,
}

impl Model {
    /// Load what is registered, start the threads, and scan everything once.
    pub fn new(storage: Storage, outbox: Sender<AppEvent>) -> Result<Model> {
        let root = storage.root().to_path_buf();
        fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

        let mut loaded = storage.load_all();
        loaded.errors.sort_by(|a, b| a.path.cmp(&b.path));

        let mut watcher = recommended_watcher(forwarder(outbox.clone()))?;
        // The handover file arrives as one more event on this same channel.
        watcher.watch(&root, RecursiveMode::NonRecursive)?;

        let mut model = Model {
            scanner: Scanner::spawn(outbox.clone()),
            ticker: Ticker::spawn(PROBE_INTERVAL, outbox.clone()),
            storage,
            outbox,
            repos: loaded.repos,
            bad_files: loaded.errors,
            watcher,
            watches: HashMap::new(),
            wedged: HashSet::new(),
        };
        model.rescan_all();
        Ok(model)
    }

    // ---- reading ------------------------------------------------------

    pub fn state(&self, id: Uuid) -> Option<&RepoState> {
        self.repos.iter().find(|s| s.id == id)
    }

    fn state_mut(&mut self, id: Uuid) -> Option<&mut RepoState> {
        self.repos.iter_mut().find(|s| s.id == id)
    }

    pub fn bad_file(&self, path: &Path) -> Option<&LoadError> {
        self.bad_files.iter().find(|e| e.path == path)
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Repos and bad files, not counting the `+ Add repo…` row a controller
    /// puts ahead of them.
    pub fn row_count(&self) -> usize {
        self.repos.len() + self.bad_files.len()
    }

    pub fn repos(&self) -> &[RepoState] {
        &self.repos
    }

    pub fn bad_files(&self) -> &[LoadError] {
        &self.bad_files
    }

    pub fn watches(&self) -> &HashMap<PathBuf, WatchEntry> {
        &self.watches
    }

    /// Say why the app could not do what a row asked. The row is the only place
    /// it can show: nothing a key does reaches the loop as an error.
    fn fail(&mut self, id: Uuid, message: String) {
        if let Some(state) = self.state_mut(id) {
            state.error = Some(message);
        }
    }

    /// Why this row is dimmed, in the order the answers displace each other: a
    /// wedged filesystem is the only one that can be current where it applies,
    /// and a shadowed repo is never scanned, so nothing a scan determined is.
    pub fn inactive_reason(&self, id: Uuid) -> Option<InactiveReason> {
        let state = self.state(id)?;
        if self.touches_wedged(state) {
            return Some(InactiveReason::NotResponding);
        }
        match state.duplicate {
            Duplicate::Shadowed => return Some(InactiveReason::Shadowed),
            Duplicate::Undecided => return Some(InactiveReason::Undecided),
            Duplicate::None | Duplicate::Scoring => {}
        }
        if state.stale {
            return Some(InactiveReason::Moved);
        }
        match state.ownership {
            Some(Ownership::Foreign) => return Some(InactiveReason::Foreign),
            Some(Ownership::Rerooted) => return Some(InactiveReason::Rerooted),
            _ => {}
        }
        if let Some(message) = &state.error {
            return Some(InactiveReason::Other(message.clone()));
        }
        state.no_email.then_some(InactiveReason::NoEmail)
    }

    /// A thread can only wedge while it is inside a call, and a scan or a
    /// resolve outstanding or a repo being probed are the only times one is.
    pub(crate) fn may_wedge(&self) -> bool {
        self.scanner.idle() && !self.repos.iter().any(|s| s.stale)
    }

    pub(crate) fn is_wedged(&self, path: &Path) -> bool {
        self.wedged.contains(path)
    }

    // ---- removing -----------------------------------------------------

    /// A state file that will not delete takes its own row rather than the
    /// caller: the row is the only place the failure can show.
    pub(crate) fn remove_repo(&mut self, id: Uuid) {
        if let Err(e) = self.storage.remove(id) {
            self.fail(id, format!("could not remove: {e:#}"));
            return;
        }
        self.unwatch_repo(id);
        let was_stale = self.state(id).is_some_and(|s| s.stale);
        self.repos.retain(|s| s.id != id);
        if was_stale {
            self.sync_probes();
        }
        self.scanner.scans.remove(&id);
    }

    pub(crate) fn remove_bad_file(&mut self, path: &Path) {
        if let Err(e) = fs::remove_file(path) {
            // The row carries no error of its own, so it says this where it
            // would have said why the file did not load.
            if let Some(bad) = self.bad_files.iter_mut().find(|b| b.path == path) {
                bad.message = format!("could not remove: {e}");
            }
            return;
        }
        self.bad_files.retain(|e| e.path != path);
    }

    // ---- registering ----------------------------------------------------

    pub(crate) fn resolve(&mut self, seq: u64, path: PathBuf, states: Vec<RepoState>) {
        self.scanner.resolve(seq, path, states);
    }

    pub(crate) fn resolved(&mut self) {
        self.scanner.resolved();
    }

    pub(crate) fn resolve_add(&mut self, result: Result<Resolved>) -> AddOutcome {
        let resolved = match result {
            Ok(resolved) => resolved,
            Err(e) => return AddOutcome::Failed(format!("{e:#}")),
        };
        match &resolved.matched {
            Match::Reuse(id) => AddOutcome::Registered(self.register_into(*id, &resolved)),
            Match::New => AddOutcome::Registered(self.register_new(&resolved)),
            Match::Selection(candidates) => {
                // A candidate at this exact store, correctly rooted, is what
                // "new" would double-count — an unrelated occupant is not.
                let store = &resolved.identity.git_common_dir;
                let already_registered = candidates.iter().any(|id| {
                    self.state(*id).is_some_and(|s| {
                        &s.git_common_dir == store
                            && share_a_root_commit(&s.root_commits, &resolved.identity.root_commits)
                    })
                });
                AddOutcome::Pick {
                    candidates: candidates.clone(),
                    offer_new: !already_registered,
                    resolved,
                }
            }
        }
    }

    pub(crate) fn register_into(&mut self, id: Uuid, resolved: &Resolved) -> Uuid {
        if let Some(i) = self.repos.iter().position(|s| s.id == id)
            && let Err(e) =
                self.storage
                    .adopt(&mut self.repos[i], &resolved.identity, &resolved.source_dir)
        {
            self.fail(id, format!("state file would not write: {e:#}"));
        }
        id
    }

    pub(crate) fn register_new(&mut self, resolved: &Resolved) -> Uuid {
        let mut state = RepoState::new(&resolved.identity, &resolved.source_dir);
        let id = state.id;
        if let Err(e) = self.storage.save(&state) {
            state.error = Some(format!("state file would not write: {e:#}"));
        }
        // Newest `added_at`, so the load order is kept by appending.
        self.repos.push(state);
        id
    }

    // ---- scanning ---------------------------------------------------------

    /// Every repo, and a fresh try at every path that stopped answering: `r` is
    /// the manual refresh, so it is the way back from one that has recovered.
    pub(crate) fn rescan_all(&mut self) {
        if !self.wedged.is_empty() {
            self.wedged.clear();
            self.sync_probes();
        }
        let ids: Vec<Uuid> = self.repos.iter().map(|s| s.id).collect();
        for id in ids {
            self.queue_scan(id);
        }
    }

    pub(crate) fn queue_scan(&mut self, id: Uuid) {
        let Some(state) = self.repos.iter().find(|s| s.id == id) else {
            return;
        };
        if matches!(state.duplicate, Duplicate::Shadowed | Duplicate::Undecided) {
            return;
        }
        // `inactive_reason` reads the same paths, so the row already says so.
        if self.touches_wedged(state) {
            return;
        }
        // A row we do not hold as ours has to be asked about: the cheap path
        // returns without asking, and the answer that dimmed it would stand.
        let verify_ownership =
            state.stale || !matches!(state.ownership, None | Some(Ownership::Ours));
        self.scanner.scan(state.clone(), verify_ownership);
    }

    pub(crate) fn on_scan_done(
        &mut self,
        incarnation: u64,
        id: Uuid,
        result: Result<ScanFindings>,
    ) {
        // From a worker that was abandoned. `check_wedged` re-posted the job it
        // never answered, so banking this too would pay for its commits twice.
        if incarnation != self.scanner.incarnation {
            return;
        }
        let again = self.scanner.scanned(id);
        if self.state(id).is_none() {
            return; // removed while the job was out
        }
        match result {
            Ok(findings) => self.bank(id, findings),
            Err(e) => self.fail(id, format!("last scan failed: {e:#}")),
        }
        if again {
            self.queue_scan(id);
        }
    }

    /// A worktree's new location resolved. `Ok` adopts it, the same call a
    /// manual re-add makes — no `match_identity`, since a same-parent rename
    /// preserves the inode: which repo this is was never in question. Either
    /// way a rescan follows, to bank anything scored during the move and (on
    /// `Err`, the path having moved again before this resolved) fall back to
    /// the plain "gone" path staleness already answers.
    pub(crate) fn on_relocated(
        &mut self,
        incarnation: u64,
        id: Uuid,
        new_source_dir: PathBuf,
        result: Result<RepoIdentity>,
    ) {
        if incarnation != self.scanner.incarnation {
            return;
        }
        self.scanner.relocated();
        let Some(i) = self.repos.iter().position(|s| s.id == id) else {
            return; // removed while the job was out
        };
        if let Ok(identity) = result {
            if let Err(e) = self
                .storage
                .adopt(&mut self.repos[i], &identity, &new_source_dir)
            {
                self.fail(id, format!("state file would not write: {e:#}"));
            }
            self.refresh_parent_watches(id);
        }
        self.queue_scan(id);
    }

    fn bank(&mut self, id: Uuid, findings: ScanFindings) {
        let Some(i) = self.repos.iter().position(|s| s.id == id) else {
            return;
        };
        self.repos[i].error = None;

        if findings.stale {
            self.unwatch_repo(id);
            self.repos[i].ownership = None;
            self.set_stale(i, true);
            return;
        }

        if let Some(ownership) = findings.ownership {
            self.repos[i].ownership = Some(ownership);
        }
        // Only what this scan resolved moves a row between the watcher and the
        // ticker. What a previous one did is not an answer about this store.
        match findings.ownership {
            // A clone still writing its refs. The probe asks again in a second.
            Some(Ownership::NotYet) => return,
            // The store is there but is not this repo's, so there is nothing
            // left to watch and nothing further to probe for.
            Some(Ownership::Foreign | Ownership::Rerooted) => {
                self.unwatch_repo(id);
                self.set_stale(i, false);
                return;
            }
            Some(Ownership::Ours) => self.set_stale(i, false),
            // Never asked, which is most scans. A live row goes on as it was;
            // a stale one is not taken back on findings that did not look.
            None if self.repos[i].stale => return,
            None => {}
        }

        self.set_watches(id, &findings.watch);
        if let Some(missing) = findings.no_email {
            self.repos[i].no_email = missing;
        }

        scan::apply(&mut self.repos[i], findings);
        // A total that stopped persisting is the failure the whole design is
        // against, so it takes the row rather than passing unseen.
        if let Err(e) = self.storage.save(&self.repos[i]) {
            self.repos[i].error = Some(format!("state file would not write: {e:#}"));
        }
    }

    /// Move a row between the watcher's business and the ticker's, handing the
    /// ticker its new list only where the row actually crossed.
    fn set_stale(&mut self, i: usize, stale: bool) {
        if self.repos[i].stale != stale {
            self.repos[i].stale = stale;
            self.sync_probes();
        }
    }

    // ---- watches and probes -----------------------------------------------

    /// A takeover file appearing is the caller's signal to quit; everything
    /// else here just decides which repo to queue or relocate.
    ///
    /// A `notify::Event`'s paths arrive together in one call, one call per
    /// event: a rename `notify` could correlate carries both the old and new
    /// path, still paired, which is what lets a same-parent rename be told
    /// from a plain one-path change (a write, a delete, a move to elsewhere).
    pub(crate) fn on_fs(&mut self, paths: Vec<PathBuf>) -> bool {
        if paths.iter().any(|p| self.is_takeover(p)) {
            return true;
        }

        // Matched against `worktrees` directly, not the watch table: the lone
        // "from" half of this same rename typically lands first (below) and
        // can mark the row stale before this, its correlated pair, arrives —
        // dropping the watch table's own entry along with it. A repo's
        // recorded path outlives that, so this still finds it and, once the
        // relocate lands, the rescan it queues clears the staleness again.
        if let [old, new] = paths.as_slice()
            && let Some(id) = self
                .repos
                .iter()
                .find(|s| s.worktrees.iter().any(|w| &w.source_dir == old))
                .map(|s| s.id)
        {
            self.relocate(id, new.clone());
            return false;
        }

        let mut touched = HashSet::new();
        for path in &paths {
            let Some(name) = path.file_name() else {
                continue;
            };
            match path.parent().and_then(|dir| self.watches.get(dir)) {
                // `logs` for the git-dir fallback, where the first commit
                // creates that directory. `HEAD.lock` passes neither.
                Some(WatchEntry::Reflog(owners)) if name == "HEAD" || name == "logs" => {
                    touched.extend(owners.iter().copied());
                }
                Some(WatchEntry::Parent(names)) => {
                    if let Some(&id) = names.get(name) {
                        touched.insert(id);
                    }
                }
                _ => {}
            }
        }
        for id in touched {
            self.queue_scan(id);
        }
        false
    }

    fn is_takeover(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == ".takeover")
            && path.parent() == Some(self.storage.root())
            // The asker removes before it creates, so the removal is an event too.
            && self.storage.takeover_path().exists()
    }

    /// Post the worker a fresh identity resolve for a worktree's new location,
    /// so `on_relocated` can adopt it the way a manual re-add would.
    fn relocate(&mut self, id: Uuid, new_source_dir: PathBuf) {
        self.scanner.relocate(id, new_source_dir);
    }

    /// Bring both watch kinds for this repo in line with its current state:
    /// `reflog_dirs` from the scan that just resolved them — deciding `logs`
    /// against the git-dir fallback is a `stat`, still the worker's answer —
    /// and the parent-of-each-worktree dirs, recomputed here since they're
    /// pure path arithmetic on `worktrees` and need no `stat` at all.
    fn set_watches(&mut self, id: Uuid, reflog_dirs: &[PathBuf]) {
        self.unwatch_reflog(id);
        for dir in reflog_dirs {
            let is_new = !self.watches.contains_key(dir);
            if let WatchEntry::Reflog(owners) = self
                .watches
                .entry(dir.clone())
                .or_insert_with(|| WatchEntry::Reflog(HashSet::new()))
            {
                owners.insert(id);
            }
            if is_new {
                let _ = self.watcher.watch(dir, RecursiveMode::NonRecursive);
            }
        }
        self.refresh_parent_watches(id);
    }

    /// Re-register a repo's parent watches against its current `worktrees`.
    /// Registered for every entry, live or not — `watch()` on a directory
    /// that isn't there just fails, the same tolerance the reflog watch's
    /// git-dir fallback already relies on.
    fn refresh_parent_watches(&mut self, id: Uuid) {
        self.unwatch_parent(id);
        let Some(state) = self.state(id) else {
            return;
        };
        let targets: Vec<(PathBuf, OsString)> = state
            .worktrees
            .iter()
            .filter_map(|w| {
                let dir = w.source_dir.parent()?.to_path_buf();
                let name = w.source_dir.file_name()?.to_os_string();
                Some((dir, name))
            })
            .collect();

        for (dir, name) in targets {
            let is_new = !self.watches.contains_key(&dir);
            if let WatchEntry::Parent(names) = self
                .watches
                .entry(dir.clone())
                .or_insert_with(|| WatchEntry::Parent(HashMap::new()))
            {
                names.insert(name, id);
            }
            if is_new {
                let _ = self.watcher.watch(&dir, RecursiveMode::NonRecursive);
            }
        }
    }

    fn unwatch_repo(&mut self, id: Uuid) {
        self.unwatch_reflog(id);
        self.unwatch_parent(id);
    }

    fn unwatch_reflog(&mut self, id: Uuid) {
        let mut orphaned = Vec::new();
        self.watches.retain(|dir, entry| {
            let WatchEntry::Reflog(owners) = entry else {
                return true;
            };
            owners.remove(&id);
            if owners.is_empty() {
                orphaned.push(dir.clone());
                return false;
            }
            true
        });
        for dir in orphaned {
            // A path that is already gone answers with an error, which is the
            // state we were asking for.
            let _ = self.watcher.unwatch(&dir);
        }
    }

    fn unwatch_parent(&mut self, id: Uuid) {
        let mut orphaned = Vec::new();
        self.watches.retain(|dir, entry| {
            let WatchEntry::Parent(names) = entry else {
                return true;
            };
            names.retain(|_, owner| *owner != id);
            if names.is_empty() {
                orphaned.push(dir.clone());
                return false;
            }
            true
        });
        for dir in orphaned {
            let _ = self.watcher.unwatch(&dir);
        }
    }

    fn sync_probes(&self) {
        let probes: Vec<Probe> = self
            .repos
            .iter()
            .filter(|state| state.stale && !self.wedged.contains(&state.git_common_dir))
            .map(|state| Probe {
                id: state.id,
                git_common_dir: state.git_common_dir.clone(),
            })
            .collect();
        self.ticker.set_probes(probes);
    }

    // ---- wedged threads ---------------------------------------------------

    /// Abandon and replace any thread wedged past [`WEDGE_LIMIT`], re-posting
    /// the work it lost. Returns the paths that wedged and match no tracked
    /// repo — a typed path belongs to none, so the caller reports it wherever
    /// such a path is shown.
    pub(crate) fn check_wedged(&mut self) -> Vec<PathBuf> {
        let mut unowned = Vec::new();
        if let Some(path) = self.scanner.worker.wedged(WEDGE_LIMIT) {
            if !self.mark_not_responding(&path) {
                unowned.push(path);
            }
            // Every job with no findings goes again — less the one that named this
            // path, which `queue_scan` now refuses.
            for id in self.scanner.replace(self.outbox.clone()) {
                self.queue_scan(id);
            }
        }

        if let Some(path) = self.ticker.wedged(WEDGE_LIMIT) {
            if !self.mark_not_responding(&path) {
                unowned.push(path);
            }
            let fresh = Ticker::spawn(PROBE_INTERVAL, self.outbox.clone());
            mem::replace(&mut self.ticker, fresh).abandon();
            self.sync_probes();
        }
        unowned
    }

    /// Take this path out of every thread's reach, and say whether any row is
    /// on it. A typed one belongs to none, so it has nowhere but the prompt to
    /// be reported. `inactive_reason` reads `wedged`, so the rows need no marking here.
    pub(crate) fn mark_not_responding(&mut self, path: &Path) -> bool {
        self.wedged.insert(path.to_path_buf());
        self.repos.iter().any(|state| path_of(state, path))
    }

    fn touches_wedged(&self, state: &RepoState) -> bool {
        self.wedged.iter().any(|path| path_of(state, path))
    }
}

#[cfg(test)]
impl Model {
    /// Test scaffolding: a bad file registered without touching disk, for
    /// tests that only need the row it produces.
    pub(crate) fn inject_bad_file(&mut self, error: LoadError) {
        self.bad_files.push(error);
    }

    /// Whether a scan for this repo is out with the worker.
    pub(crate) fn is_scanning(&self, id: Uuid) -> bool {
        self.scanner.scans.contains_key(&id)
    }

    /// Resolves posted with no answer back yet.
    pub(crate) fn unresolved_paths(&self) -> usize {
        self.scanner.unresolved_paths
    }

    /// Relocates posted with no answer back yet.
    pub(crate) fn relocating(&self) -> usize {
        self.scanner.relocating
    }
}

/// Whether `path` is one of the places this repo makes a thread touch. A typed
/// path that wedges belongs to no repo, and only costs the thread.
fn path_of(state: &RepoState, path: &Path) -> bool {
    state.git_common_dir == path || state.worktrees.iter().any(|w| w.source_dir == path)
}

/// `notify` calls this on its own thread. A dropped error is a watch that has
/// stopped delivering, which shows as a repo that only updates on `r`.
fn forwarder(outbox: Sender<AppEvent>) -> impl EventHandler {
    move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result {
            let _ = outbox.send(AppEvent::Fs(event.paths));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};

    use crate::model::git::RepoIdentity;
    use crate::model::scan::Awarded;
    use crate::model::storage::repo::Event;

    /// A model over a throwaway storage root, removed when the test ends.
    struct Harness {
        root: PathBuf,
        #[allow(dead_code)]
        inbox: Receiver<AppEvent>,
        model: Model,
    }

    impl Harness {
        /// Registered but never scanned, which is the state at startup.
        fn new(count: usize) -> Harness {
            let root = std::env::temp_dir().join(format!("gng-model-{}", Uuid::new_v4()));
            let storage = Storage::new(&root);
            for i in 0..count {
                storage
                    .save(&repo(&root, &format!("r{i}")))
                    .expect("saving");
            }
            let (outbox, inbox) = mpsc::channel();
            let model = Model::new(storage, outbox).expect("building the model");
            Harness { root, inbox, model }
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
            self.model.repos.iter().map(|s| s.id).collect()
        }

        fn id(&self, index: usize) -> Uuid {
            self.ids()[index]
        }

        fn deliver(&mut self, id: Uuid, findings: ScanFindings) {
            let incarnation = self.model.scanner.incarnation;
            self.model.on_scan_done(incarnation, id, Ok(findings));
        }

        fn common_dir(&self, id: Uuid) -> PathBuf {
            self.model
                .state(id)
                .expect("the repo")
                .git_common_dir
                .clone()
        }

        /// Whether the ticker, rather than the watcher, is what covers this row.
        fn stale(&self, id: Uuid) -> bool {
            self.model.state(id).expect("the repo").stale
        }

        /// Whether a scan for this repo is out with the worker.
        fn scanning(&self, id: Uuid) -> bool {
            self.model.scanner.scans.contains_key(&id)
        }

        fn saved(&self, id: Uuid) -> Vec<u8> {
            let path = self.root.join("repos").join(format!("{id}.json"));
            fs::read(path).expect("reading a state file")
        }

        /// The directories watched for a `HEAD` write — every repo here also
        /// gets a parent watch, which most of these tests aren't about.
        fn reflog_watches(&self) -> Vec<PathBuf> {
            self.model
                .watches
                .iter()
                .filter(|(_, entry)| matches!(entry, WatchEntry::Reflog(_)))
                .map(|(dir, _)| dir.clone())
                .collect()
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

    fn earning(xp: u64) -> ScanFindings {
        ScanFindings {
            events: vec![Event {
                sha: "a0f0365".into(),
                timestamp: 1756100000,
                subject: "feat: parse reflog".into(),
                added: 120,
                deleted: 34,
                xp,
                rewrite: false,
            }],
            new_awarded: vec![Awarded {
                sha: "a0f0365".into(),
                xp,
            }],
            ownership: Some(Ownership::Ours),
            ..ScanFindings::default()
        }
    }

    fn owned(ownership: Ownership) -> ScanFindings {
        ScanFindings {
            ownership: Some(ownership),
            ..ScanFindings::default()
        }
    }

    fn gone() -> ScanFindings {
        ScanFindings {
            stale: true,
            ..ScanFindings::default()
        }
    }

    #[test]
    fn findings_bank_their_xp_and_write_that_state_file() {
        let mut harness = Harness::settled(2);
        let (earner, idle) = (harness.id(0), harness.id(1));
        let untouched = harness.saved(idle);

        harness.deliver(earner, earning(222));

        assert_eq!(harness.model.state(earner).expect("the repo").total_xp, 222);
        let written: RepoState =
            serde_json::from_slice(&harness.saved(earner)).expect("re-reading the state file");
        assert_eq!(written.total_xp, 222);
        assert_eq!(harness.saved(idle), untouched);
    }

    #[test]
    fn the_events_cap_does_not_cost_the_total() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);

        for _ in 0..250 {
            harness.deliver(id, earning(1));
        }

        let state = harness.model.state(id).expect("the repo");
        assert_eq!(state.events.len(), crate::model::storage::repo::MAX_EVENTS);
        assert_eq!(state.total_xp, 250);
    }

    #[test]
    fn a_failed_scan_leaves_the_total_alone_and_takes_the_row() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, earning(222));

        let incarnation = harness.model.scanner.incarnation;
        harness.model.on_scan_done(
            incarnation,
            id,
            Err(anyhow::anyhow!("fatal: not a git repository")),
        );

        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
        assert_eq!(
            harness.model.inactive_reason(id).map(|m| m.to_string()),
            Some("last scan failed: fatal: not a git repository".into())
        );
    }

    #[test]
    fn a_store_that_is_gone_keeps_its_total_and_is_probed_for() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, earning(222));

        harness.deliver(id, gone());

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::Moved)
        );
        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
        assert!(harness.stale(id));
    }

    #[test]
    fn a_store_that_comes_back_as_ours_un_stales_and_scores() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(id, earning(222));

        assert_eq!(harness.model.inactive_reason(id), None);
        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
        assert!(!harness.stale(id));
    }

    #[test]
    fn a_different_project_at_the_path_stops_being_probed_for() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(id, owned(Ownership::Foreign));

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::Foreign)
        );
        assert!(!harness.stale(id));
    }

    #[test]
    fn a_rewritten_history_asks_to_be_added_again() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, earning(222));

        harness.deliver(id, owned(Ownership::Rerooted));

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::Rerooted)
        );
        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
    }

    #[test]
    fn a_clone_still_writing_its_refs_leaves_the_row_stale() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(id, owned(Ownership::NotYet));

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::Moved)
        );
        assert!(harness.stale(id));
    }

    #[test]
    fn a_scan_that_never_asked_leaves_the_no_email_answer_standing() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(
            id,
            ScanFindings {
                no_email: Some(true),
                ownership: Some(Ownership::Ours),
                ..ScanFindings::default()
            },
        );

        harness.deliver(id, ScanFindings::default());

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::NoEmail)
        );
    }

    #[test]
    fn a_filesystem_that_stopped_answering_outranks_every_other_reason() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let common_dir = harness.common_dir(id);
        harness.deliver(id, gone());

        harness.model.mark_not_responding(&common_dir);

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::NotResponding)
        );
    }

    #[test]
    fn a_wedged_path_drops_that_repo_and_leaves_the_rest_queued() {
        let mut harness = Harness::settled(2);
        let (wedged, other) = (harness.id(0), harness.id(1));
        let common_dir = harness.common_dir(wedged);

        harness.model.mark_not_responding(&common_dir);
        for id in [wedged, other] {
            harness.model.queue_scan(id);
        }

        assert!(!harness.scanning(wedged));
        assert!(harness.scanning(other));
    }

    #[test]
    fn a_row_on_a_blocked_path_says_so_rather_than_going_quiet() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.model.wedged.insert(harness.common_dir(id));

        harness.model.queue_scan(id);

        assert!(!harness.scanning(id));
        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::NotResponding)
        );
    }

    #[test]
    fn a_repo_dirtied_while_its_job_is_out_is_queued_again() {
        let mut harness = Harness::new(1);
        let id = harness.id(0);
        assert!(harness.scanning(id));

        harness.model.queue_scan(id);
        harness.deliver(id, ScanFindings::default());

        assert!(harness.scanning(id));
    }

    #[test]
    fn a_store_that_returned_is_not_taken_back_by_a_scan_that_never_asked() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(
            id,
            ScanFindings {
                watch: vec![harness.root.join("r0/.git/logs")],
                ..ScanFindings::default()
            },
        );

        assert_eq!(
            harness.model.inactive_reason(id),
            Some(InactiveReason::Moved)
        );
        assert!(harness.stale(id));
        assert!(harness.model.watches.is_empty());
    }

    #[test]
    fn findings_from_a_worker_that_was_abandoned_are_dropped() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, earning(222));
        let superseded = harness.model.scanner.incarnation;
        // As `check_wedged` replaces it, having re-posted the job itself.
        harness.model.scanner.incarnation += 1;

        harness.model.on_scan_done(superseded, id, Ok(earning(222)));

        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
    }

    #[test]
    fn a_head_written_under_a_watched_dir_scans_that_repo() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let logs = harness.root.join("r0/.git/logs");
        harness.deliver(
            id,
            ScanFindings {
                watch: vec![logs.clone()],
                ..ScanFindings::default()
            },
        );

        harness.model.on_fs(vec![logs.join("HEAD")]);

        assert!(harness.scanning(id));
    }

    /// A repo with no commits is watched at its git dir, and its first commit
    /// creates `logs` rather than writing anything called `HEAD`.
    #[test]
    fn the_logs_dir_appearing_scans_that_repo() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let git_dir = harness.root.join("r0/.git");
        harness.deliver(
            id,
            ScanFindings {
                watch: vec![git_dir.clone()],
                ..ScanFindings::default()
            },
        );

        harness.model.on_fs(vec![git_dir.join("logs")]);

        assert!(harness.scanning(id));
    }

    #[test]
    fn the_lock_churn_beside_a_reflog_scans_nothing() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let logs = harness.root.join("r0/.git/logs");
        harness.deliver(
            id,
            ScanFindings {
                watch: vec![logs.clone()],
                ..ScanFindings::default()
            },
        );

        harness.model.on_fs(vec![logs.join("HEAD.lock")]);

        assert!(harness.model.scanner.scans.is_empty());
    }

    #[test]
    fn a_same_parent_rename_dispatches_a_relocate() {
        let mut harness = Harness::settled(1);
        let old = harness.root.join("r0");
        let new = harness.root.join("r0-renamed");

        harness.model.on_fs(vec![old, new]);

        assert_eq!(harness.model.relocating(), 1);
    }

    #[test]
    fn an_unrelated_rename_beside_a_parent_watch_scans_nothing() {
        let mut harness = Harness::settled(1);
        let old = harness.root.join("someone-elses-dir");
        let new = harness.root.join("someone-elses-dir-renamed");

        harness.model.on_fs(vec![old, new]);

        assert!(harness.model.scanner.scans.is_empty());
    }

    #[test]
    fn a_lone_departure_under_a_parent_watch_queues_a_rescan() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);

        harness.model.on_fs(vec![harness.root.join("r0")]);

        assert!(harness.scanning(id));
    }

    #[test]
    fn a_relocate_adopts_the_new_path_and_queues_a_rescan() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let new_dir = harness.root.join("r0-renamed");
        let identity = RepoIdentity {
            root_commits: vec!["r0-root".into()],
            git_common_dir: new_dir.join(".git"),
            head_subdir: PathBuf::new(),
        };
        let incarnation = harness.model.scanner.incarnation;

        harness
            .model
            .on_relocated(incarnation, id, new_dir.clone(), Ok(identity));

        let state = harness.model.state(id).expect("the repo");
        assert_eq!(state.git_common_dir, new_dir.join(".git"));
        assert_eq!(state.worktrees[0].source_dir, new_dir);
        assert!(!state.stale); // never dipped stale for a resolved relocate
        assert!(harness.scanning(id)); // the reconciling rescan is out
    }

    #[test]
    fn a_relocate_that_fails_falls_back_to_a_rescan_without_failing_the_row() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let incarnation = harness.model.scanner.incarnation;

        harness.model.on_relocated(
            incarnation,
            id,
            harness.root.join("wherever-it-went-next"),
            Err(anyhow::anyhow!("moved again before this resolved")),
        );

        assert!(harness.model.state(id).expect("the repo").error.is_none());
        assert!(harness.scanning(id));
    }

    #[test]
    fn findings_replace_the_watches_the_ones_before_them_named() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let (first, second) = (
            harness.root.join("r0/.git/logs"),
            harness.root.join("moved"),
        );
        fs::create_dir_all(&second).expect("creating the new dir");

        harness.deliver(
            id,
            ScanFindings {
                watch: vec![first.clone()],
                ..ScanFindings::default()
            },
        );
        harness.deliver(
            id,
            ScanFindings {
                watch: vec![second.clone()],
                ..ScanFindings::default()
            },
        );

        assert_eq!(harness.reflog_watches(), vec![second]);
    }

    /// The one test that uses `notify` for real: everything above hands the app
    /// the paths a watch would have delivered.
    #[test]
    fn a_registered_watch_delivers_what_is_written_under_it() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let logs = harness.root.join("r0/.git/logs");
        fs::create_dir_all(&logs).expect("creating the logs dir");
        harness.deliver(
            id,
            ScanFindings {
                watch: vec![logs.clone()],
                ..ScanFindings::default()
            },
        );

        fs::write(logs.join("HEAD"), b"0000000 215c9bf\n").expect("writing the reflog");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match harness.inbox.recv_timeout(left) {
                Ok(AppEvent::Fs(paths)) if paths.iter().any(|p| p.ends_with("HEAD")) => break,
                Ok(_) => continue,
                Err(e) => panic!("no event for the reflog write: {e}"),
            }
        }
    }

    /// A second use of a real `notify` watcher: a same-parent rename arrives
    /// as one event carrying both paths, which is what lets `on_fs` follow it
    /// instead of only noticing it left.
    #[test]
    fn a_same_parent_rename_is_delivered_as_a_correlated_pair() {
        let harness = Harness::settled(1);
        let old = harness.root.join("r0");
        let new = harness.root.join("r0-renamed");
        fs::create_dir_all(&old).expect("creating the worktree dir");

        fs::rename(&old, &new).expect("renaming it in place");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match harness.inbox.recv_timeout(left) {
                Ok(AppEvent::Fs(paths)) if paths.as_slice() == [old.clone(), new.clone()] => break,
                Ok(_) => continue,
                Err(e) => panic!("no correlated rename event: {e}"),
            }
        }
    }
}
