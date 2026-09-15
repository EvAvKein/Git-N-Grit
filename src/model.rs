//! The domain state: every repo in memory, storage, and the threads that keep
//! them live.
//!
//! Nothing here knows what is on screen. Liveness, `git` and every `stat` are
//! the [worker](crate::scan)'s and the [ticker](crate::stale)'s answers,
//! arriving as [`AppEvent`]s; this decides what they mean for a repo.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventHandler, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use uuid::Uuid;

use crate::event::AppEvent;
use crate::scan::{self, Job, Resolved, ScanFindings, Worker};
use crate::stale::{Probe, Ticker};
use crate::state::{Duplicate, LoadError, Match, Ownership, RepoState, Storage};

/// A filesystem call outstanding this long is one nothing can free.
pub const WEDGE_LIMIT: Duration = Duration::from_secs(10);

/// How often a store that has gone missing is stat-ed for its return.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

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

/// Whether the scan out for a repo has been overtaken since it was posted. An
/// enum rather than a `bool` so the answer reads where it is asked.
#[derive(Debug, PartialEq, Eq)]
enum ScanOutdated {
    False,
    /// A scan was asked for while this one was out — a reflog write, an `r`, a
    /// store that returned. The job carries a clone of the state as it was when
    /// it went out, so its findings cannot account for whatever changed since.
    True,
}

/// The worker, and what has been posted to it with no answer back yet.
struct Scanner {
    worker: Worker,
    /// Which spawn of the worker this is. Stamped on every scan's findings, so
    /// those from a worker abandoned since are told from the job that replaced it.
    incarnation: u64,
    jobs: Sender<Job>,
    scans: HashMap<Uuid, ScanOutdated>,
    /// Paths posted with no answer back yet — one that failed still answers.
    /// Never read for its value, only for whether it is zero.
    unresolved_paths: usize,
}

impl Scanner {
    fn spawn(outbox: Sender<AppEvent>) -> Scanner {
        let (worker, jobs) = Worker::spawn(outbox, 0);
        Scanner {
            worker,
            incarnation: 0,
            jobs,
            scans: HashMap::new(),
            unresolved_paths: 0,
        }
    }

    /// Nothing posted and unanswered, so no thread can be inside a call.
    fn idle(&self) -> bool {
        self.scans.is_empty() && self.unresolved_paths == 0
    }

    /// Post a scan, or record that the one already out has been overtaken. Its
    /// findings re-queue the repo rather than racing the job that would have.
    fn scan(&mut self, state: RepoState, verify_ownership: bool) {
        match self.scans.entry(state.id) {
            Entry::Occupied(mut out) => {
                out.insert(ScanOutdated::True);
            }
            Entry::Vacant(slot) => {
                slot.insert(ScanOutdated::False);
                let _ = self.jobs.send(Job::Scan {
                    state: Box::new(state),
                    verify_ownership,
                });
            }
        }
    }

    /// Findings landed. Whether that repo needs scanning again.
    fn scanned(&mut self, id: Uuid) -> bool {
        self.scans.remove(&id) == Some(ScanOutdated::True)
    }

    fn resolve(&mut self, seq: u64, path: PathBuf, states: Vec<RepoState>) {
        self.unresolved_paths += 1;
        let _ = self.jobs.send(Job::Resolve { seq, path, states });
    }

    /// Answered, whether or not the prompt still wants it. Saturating because
    /// [`replace`](Scanner::replace) zeroes the count, and a `Resolved` already
    /// sent and still unread arrives after it.
    fn resolved(&mut self) {
        self.unresolved_paths = self.unresolved_paths.saturating_sub(1);
    }

    /// Abandon a wedged worker for one whose jobs leave that path out. The
    /// returned ids have to be posted again: the old worker took its queue
    /// down with it, and answers nothing it had not already sent.
    fn replace(&mut self, outbox: Sender<AppEvent>) -> Vec<Uuid> {
        self.incarnation += 1;
        let (worker, jobs) = Worker::spawn(outbox, self.incarnation);
        // Dropping the old queue is what unblocks a worker still waiting on it.
        self.jobs = jobs;
        mem::replace(&mut self.worker, worker).abandon();
        self.unresolved_paths = 0;
        self.scans.drain().map(|(id, _)| id).collect()
    }
}

pub struct Model {
    storage: Storage,
    outbox: Sender<AppEvent>,

    repos: Vec<RepoState>,
    bad_files: Vec<LoadError>,

    watcher: RecommendedWatcher,
    watches: HashMap<PathBuf, HashSet<Uuid>>,

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

    pub fn watches(&self) -> &HashMap<PathBuf, HashSet<Uuid>> {
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
    pub fn mark(&self, id: Uuid) -> Option<InactiveReason> {
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
                // A second state file on a store already registered would watch
                // that reflog alongside the first, and both would score it.
                let store = &resolved.identity.git_common_dir;
                let offer_new = !self.repos.iter().any(|s| &s.git_common_dir == store);
                AddOutcome::Pick {
                    candidates: candidates.clone(),
                    offer_new,
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
        // `mark` reads the same paths, so the row already says so.
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
    /// else here just decides which repo to queue.
    pub(crate) fn on_fs(&mut self, paths: Vec<PathBuf>) -> bool {
        let mut touched = HashSet::new();
        for path in paths {
            if self.is_takeover(&path) {
                return true;
            }
            // `logs` for the git-dir fallback, where the first commit creates
            // that directory. `HEAD.lock` passes neither.
            let Some(name) = path.file_name() else {
                continue;
            };
            if name != "HEAD" && name != "logs" {
                continue;
            }
            if let Some(owners) = path.parent().and_then(|dir| self.watches.get(dir)) {
                touched.extend(owners.iter().copied());
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

    fn set_watches(&mut self, id: Uuid, dirs: &[PathBuf]) {
        self.unwatch_repo_except(id, dirs);
        for dir in dirs {
            let owners = self.watches.entry(dir.clone()).or_default();
            owners.insert(id);
            if owners.len() == 1 {
                let _ = self.watcher.watch(dir, RecursiveMode::NonRecursive);
            }
        }
    }

    fn unwatch_repo(&mut self, id: Uuid) {
        self.unwatch_repo_except(id, &[]);
    }

    fn unwatch_repo_except(&mut self, id: Uuid, keep: &[PathBuf]) {
        let mut orphaned = Vec::new();
        self.watches.retain(|dir, owners| {
            if keep.contains(dir) {
                return true;
            }
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
    /// be reported. `mark` reads `wedged`, so the rows need no marking here.
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

    use crate::git::RepoIdentity;
    use crate::scan::Awarded;
    use crate::state::Event;

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
        assert_eq!(state.events.len(), crate::state::MAX_EVENTS);
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
            harness.model.mark(id).map(|m| m.to_string()),
            Some("last scan failed: fatal: not a git repository".into())
        );
    }

    #[test]
    fn a_store_that_is_gone_keeps_its_total_and_is_probed_for() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, earning(222));

        harness.deliver(id, gone());

        assert_eq!(harness.model.mark(id), Some(InactiveReason::Moved));
        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
        assert!(harness.stale(id));
    }

    #[test]
    fn a_store_that_comes_back_as_ours_un_stales_and_scores() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(id, earning(222));

        assert_eq!(harness.model.mark(id), None);
        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
        assert!(!harness.stale(id));
    }

    #[test]
    fn a_different_project_at_the_path_stops_being_probed_for() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(id, owned(Ownership::Foreign));

        assert_eq!(harness.model.mark(id), Some(InactiveReason::Foreign));
        assert!(!harness.stale(id));
    }

    #[test]
    fn a_rewritten_history_asks_to_be_added_again() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, earning(222));

        harness.deliver(id, owned(Ownership::Rerooted));

        assert_eq!(harness.model.mark(id), Some(InactiveReason::Rerooted));
        assert_eq!(harness.model.state(id).expect("the repo").total_xp, 222);
    }

    #[test]
    fn a_clone_still_writing_its_refs_leaves_the_row_stale() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        harness.deliver(id, gone());

        harness.deliver(id, owned(Ownership::NotYet));

        assert_eq!(harness.model.mark(id), Some(InactiveReason::Moved));
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

        assert_eq!(harness.model.mark(id), Some(InactiveReason::NoEmail));
    }

    #[test]
    fn a_filesystem_that_stopped_answering_outranks_every_other_reason() {
        let mut harness = Harness::settled(1);
        let id = harness.id(0);
        let common_dir = harness.common_dir(id);
        harness.deliver(id, gone());

        harness.model.mark_not_responding(&common_dir);

        assert_eq!(harness.model.mark(id), Some(InactiveReason::NotResponding));
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
        assert_eq!(harness.model.mark(id), Some(InactiveReason::NotResponding));
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

        assert_eq!(harness.model.mark(id), Some(InactiveReason::Moved));
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

        assert_eq!(
            harness.model.watches.keys().collect::<Vec<_>>(),
            vec![&second]
        );
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
}
