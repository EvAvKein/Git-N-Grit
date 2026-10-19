//! The scan worker, and what has been posted to it with no answer back yet.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use uuid::Uuid;

use crate::model::event::AppEvent;
use crate::model::scan::{Job, Worker};
use crate::model::storage::repo::RepoState;

/// Whether the scan out for a repo has been overtaken since it was posted. An
/// enum rather than a `bool` so the answer reads where it is asked.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScanOutdated {
    False,
    /// A scan was asked for while this one was out — a reflog write, an `r`, a
    /// store that returned. The job carries a clone of the state as it was when
    /// it went out, so its findings cannot account for whatever changed since.
    True,
}

/// The worker, and what has been posted to it with no answer back yet.
pub(super) struct Scanner {
    pub(super) worker: Worker,
    /// Which spawn of the worker this is. Stamped on every scan's findings, so
    /// those from a worker abandoned since are told from the job that replaced it.
    pub(super) incarnation: u64,
    pub(super) jobs: Sender<Job>,
    pub(super) scans: HashMap<Uuid, ScanOutdated>,
    /// Relocates posted with no answer back yet — one that failed still
    /// answers. Never read for its value, only for whether it is zero, the
    /// same shape as `unresolved_paths` and for the same reason: a
    /// same-parent rename delivers its lone "from" before the correlated pair
    /// that resolves it (below), so more than one can be out for the same
    /// repo, each spawning its own subprocess and touching nothing shared
    /// until its own result lands.
    pub(super) relocating: usize,
    /// Paths posted with no answer back yet — one that failed still answers.
    /// Never read for its value, only for whether it is zero.
    pub(super) unresolved_paths: usize,
}

impl Scanner {
    pub(super) fn spawn(outbox: Sender<AppEvent>) -> Scanner {
        let (worker, jobs) = Worker::spawn(outbox, 0);
        Scanner {
            worker,
            incarnation: 0,
            jobs,
            scans: HashMap::new(),
            relocating: 0,
            unresolved_paths: 0,
        }
    }

    /// Nothing posted and unanswered, so no thread can be inside a call.
    pub(super) fn idle(&self) -> bool {
        self.scans.is_empty() && self.relocating == 0 && self.unresolved_paths == 0
    }

    /// Post a scan, or record that the one already out has been overtaken. Its
    /// findings re-queue the repo rather than racing the job that would have.
    pub(super) fn scan(&mut self, state: RepoState, verify_ownership: bool) {
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
    pub(super) fn scanned(&mut self, id: Uuid) -> bool {
        self.scans.remove(&id) == Some(ScanOutdated::True)
    }

    pub(super) fn relocate(&mut self, id: Uuid, new_source_dir: PathBuf) {
        self.relocating += 1;
        let _ = self.jobs.send(Job::Relocate { id, new_source_dir });
    }

    /// Answered, whether or not it is still the freshest answer for this
    /// repo — `on_relocated` is what tells a stale one from a current one.
    pub(super) fn relocated(&mut self) {
        self.relocating = self.relocating.saturating_sub(1);
    }

    pub(super) fn resolve(&mut self, seq: u64, path: PathBuf, states: Vec<RepoState>) {
        self.unresolved_paths += 1;
        let _ = self.jobs.send(Job::Resolve { seq, path, states });
    }

    /// Answered, whether or not the prompt still wants it. Saturating because
    /// [`replace`](Scanner::replace) zeroes the count, and a `Resolved` already
    /// sent and still unread arrives after it.
    pub(super) fn resolved(&mut self) {
        self.unresolved_paths = self.unresolved_paths.saturating_sub(1);
    }

    /// Abandon a wedged worker for one whose jobs leave that path out. The
    /// returned ids have to be posted again: the old worker took its queue
    /// down with it, and answers nothing it had not already sent.
    pub(super) fn replace(&mut self, outbox: Sender<AppEvent>) -> Vec<Uuid> {
        self.incarnation += 1;
        let (worker, jobs) = Worker::spawn(outbox, self.incarnation);
        // Dropping the old queue is what unblocks a worker still waiting on it.
        self.jobs = jobs;
        mem::replace(&mut self.worker, worker).abandon();
        self.unresolved_paths = 0;
        self.relocating = 0;
        self.scans.drain().map(|(id, _)| id).collect()
    }
}
