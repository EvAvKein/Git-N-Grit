//! One object store's state: what it has earned, and the worktrees it is read
//! through.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::VERSION;
use crate::model::git::RepoIdentity;

/// Newest events kept per repo. Display-only: `total_xp` is its own running
/// sum, so dropping the rest costs nothing.
pub const MAX_EVENTS: usize = 200;

/// Everything one object store has earned, and everything needed to not pay for
/// it twice. A repo and every linked worktree of it share one of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    pub file_format_version: u32,
    /// Sorted, as `RepoIdentity` delivers them. Empty until a commitless repo
    /// makes its first commit.
    pub root_commits: Vec<String>,
    pub git_common_dir: PathBuf,
    pub added_at: i64,
    pub total_xp: u64,
    /// Every SHA the scan has run through scoring, keyed to the XP it was
    /// awarded — zero for a merge, another author's commit, or an object gc
    /// pruned. Kept so a total can be checked against the commits that built it.
    pub awarded: BTreeMap<String, u64>,
    /// `BTree` rather than `Hash` for both: each serializes in the same order
    /// twice, and two saves of an unchanged state are compared byte for byte.
    pub seen_patch_ids: BTreeSet<String>,
    pub worktrees: Vec<Worktree>,
    pub events: Vec<Event>,
    // Out of the file by design: persisted, any of these would be stale at startup.
    /// The filename, minus `.json`. Nothing locates a repo by it, and only
    /// `Storage` needs it to write back.
    #[serde(skip)]
    pub id: Uuid,
    /// Whether another state file names this store too. Read from the file names
    /// on disk at load.
    #[serde(skip)]
    pub duplicate: Duplicate,
    /// Whether the object store's common dir is missing from disk.
    #[serde(skip)]
    pub stale: bool,
    /// Who the store at this path belongs to. `None` until a scan has judged it.
    #[serde(skip)]
    pub ownership: Option<Ownership>,
    /// Whether `user.email` was unset, last time a scan looked.
    #[serde(skip)]
    pub no_email: bool,
    /// Why the most recent action on this repo failed.
    #[serde(skip)]
    pub error: Option<String>,
}

/// Whether one object store is named by more than one state file.
///
/// Two would both watch its reflog and both score it, so the newest file earns
/// and the others hold what they have. Merging them is not ours to do: their
/// totals were banked under histories that can no longer be told apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Duplicate {
    /// The only state file naming this store.
    #[default]
    None,
    /// Shares the store, and is the one watched and scanned.
    Scoring,
    /// Shares the store, and is neither watched nor scanned.
    Shadowed,
    /// Shares the store with a file that would not say when it was written, so
    /// which is current is unknown. Nothing here is watched or scanned: scoring
    /// into a total picked by guesswork is worse than scoring into none, and
    /// removing one file settles it.
    Undecided,
}

/// Whose the object store at a state's own path is.
///
/// A stale row whose store has reappeared asks this, and so does a live row
/// before a scan banks anything into its total: a path can be refilled by an
/// unrelated project between one scan and the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// This state's. Score it, and take it back where it had gone.
    Ours,
    /// Ours, but its root commits have changed since we recorded them.
    Rerooted,
    /// Nothing yet says what it is, so ask again rather than deciding.
    NotYet,
    /// A different project's. Nothing here is scored into this state's total.
    Foreign,
}

/// One registered worktree of the store: where its reflog is, and where its
/// files are. Neither yields the other by string manipulation, so both are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Locates the dir holding this worktree's own `HEAD`, and so its
    /// `logs/HEAD`. Relative to `git_common_dir`: empty for the main worktree,
    /// `worktrees/<name>` for a linked one.
    pub head_subdir: PathBuf,
    /// Where the files are, absolute. Where `git config user.email` is asked,
    /// and what the left pane labels the row from.
    pub source_dir: PathBuf,
}

/// One scored commit, for the activity pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub sha: String,
    pub timestamp: i64,
    pub subject: String,
    pub added: u64,
    pub deleted: u64,
    pub xp: u64,
    pub rewrite: bool,
}

impl RepoState {
    /// A store registered for the first time, from `source_dir`.
    pub fn new(identity: &RepoIdentity, source_dir: &Path) -> Self {
        RepoState {
            file_format_version: VERSION,
            root_commits: identity.root_commits.clone(),
            git_common_dir: identity.git_common_dir.clone(),
            added_at: now(),
            total_xp: 0,
            awarded: BTreeMap::new(),
            seen_patch_ids: BTreeSet::new(),
            worktrees: vec![Worktree {
                head_subdir: identity.head_subdir.clone(),
                source_dir: source_dir.to_path_buf(),
            }],
            events: Vec::new(),
            id: Uuid::new_v4(),
            duplicate: Duplicate::None,
            stale: false,
            ownership: None,
            no_email: false,
            error: None,
        }
    }

    /// Fold a resolved identity into this store: a moved common dir, roots
    /// gained since, and the worktree it was added by — appended where it is
    /// new, refreshed in place where it is already listed.
    pub fn adopt(&mut self, identity: &RepoIdentity, source_dir: &Path) {
        // A commitless repo resolves no roots; keeping the stored ones is what
        // lets it match itself before its first scan.
        if !identity.root_commits.is_empty() {
            self.root_commits = identity.root_commits.clone();
        }
        self.git_common_dir = identity.git_common_dir.clone();

        // The main worktree's head_subdir is empty on both sides, so a moved
        // repo matches here: a skip would leave it holding the path it left.
        match self
            .worktrees
            .iter_mut()
            .find(|w| w.head_subdir == identity.head_subdir)
        {
            Some(w) => w.source_dir = source_dir.to_path_buf(),
            None => self.worktrees.push(Worktree {
                head_subdir: identity.head_subdir.clone(),
                source_dir: source_dir.to_path_buf(),
            }),
        }
    }

    /// Bank a scan's events, oldest first, and the XP they carry. Returns what
    /// was banked, which `total_xp` keeps whole however many events the cap
    /// drops here.
    pub fn record(&mut self, events: impl IntoIterator<Item = Event>) -> u64 {
        let mut earned = 0;
        for event in events {
            earned += event.xp;
            self.events.push(event);
        }
        self.total_xp += earned;

        let excess = self.events.len().saturating_sub(MAX_EVENTS);
        self.events.drain(..excess);
        earned
    }

    /// `head_subdir` resolved against the common dir, absolute.
    pub fn head_dir(&self, worktree: &Worktree) -> PathBuf {
        self.git_common_dir.join(&worktree.head_subdir)
    }

    /// The object store is gone: keeps its total, earns nothing further.
    ///
    /// Stats, so the scan worker calls it and the main thread never does: a
    /// path that has stopped answering keeps whichever thread touched it.
    /// Acting on a *change* in it belongs to startup, an add and `r`, each of
    /// which registers or drops the watch that goes with the answer.
    pub fn is_stale(&self) -> bool {
        !self.git_common_dir.exists()
    }

    /// Worktrees whose git dir still resolves — the watch and scan set. Stats
    /// where `is_stale` does, and belongs to the same thread.
    pub fn live_worktrees(&self) -> impl Iterator<Item = &Worktree> {
        self.worktrees.iter().filter(|w| self.head_dir(w).exists())
    }

    /// The directory git is *run in* for this store, which is not where its
    /// reflogs are read from: the first live worktree whose files are still
    /// there, else the store itself, which answers `config`, `log` and
    /// `patch-id` with no working tree at all.
    ///
    /// A worktree's git dir can outlive its files — a hand-deleted linked
    /// worktree, or `--separate-git-dir` whose tree was removed — and every
    /// `git -C` against a directory that is gone exits 128.
    pub fn scan_dir(&self) -> &Path {
        self.live_worktrees()
            .map(|w| w.source_dir.as_path())
            .find(|dir| dir.exists())
            .unwrap_or(&self.git_common_dir)
    }

    /// Whose the store now at this state's path is.
    pub fn ownership(&self, identity: &RepoIdentity, holds_seen_commits: bool) -> Ownership {
        // A state holding no roots has never scored: the first scan to find one
        // adopts it, so the common dir is all there is to match on.
        if self.root_commits.is_empty() {
            if self.git_common_dir == identity.git_common_dir {
                return Ownership::Ours;
            }
            return Ownership::Foreign;
        }

        // `git clone` creates `.git` seconds before it writes a ref, so a
        // resolve inside that window finds no roots to compare.
        if identity.root_commits.is_empty() {
            return Ownership::NotYet;
        }

        for root in &identity.root_commits {
            if self.root_commits.contains(root) {
                return Ownership::Ours;
            }
        }

        // A store deleted and refilled holds none of what we scored; one whose
        // history was rewritten in place still does.
        if holds_seen_commits {
            Ownership::Rerooted
        } else {
            Ownership::Foreign
        }
    }

    /// The directory a row is labelled from: the main worktree's, or the first
    /// listed where only linked worktrees are registered.
    pub fn label_dir(&self) -> Option<&Path> {
        let main = self
            .worktrees
            .iter()
            .find(|w| w.head_subdir.as_os_str().is_empty());
        main.or(self.worktrees.first())
            .map(|w| w.source_dir.as_path())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::model::storage::tests::{ROOT_A, ROOT_B, ROOT_C, TempRoot, identity, state};

    /// Commit `n` of a run, worth 1 xp.
    fn event(n: u64) -> Event {
        Event {
            sha: format!("{n:040x}"),
            timestamp: 1756100000 + n as i64,
            subject: format!("commit {n}"),
            added: 1,
            deleted: 0,
            xp: 1,
            rewrite: false,
        }
    }

    #[test]
    fn adopt_appends_a_worktree_once() {
        let mut repo = state(&[ROOT_A], "/repo/.git");
        let linked = identity(&[ROOT_A], "/repo/.git", "worktrees/feature");

        repo.adopt(&linked, Path::new("/feature"));
        repo.adopt(&linked, Path::new("/feature"));

        assert_eq!(repo.worktrees.len(), 2);
        assert_eq!(repo.worktrees[1].source_dir, PathBuf::from("/feature"));
    }

    #[test]
    fn adopt_refreshes_a_moved_common_dir() {
        let mut repo = state(&[ROOT_A], "/old/.git");
        repo.adopt(
            &identity(&[ROOT_A, ROOT_C], "/new/.git", ""),
            Path::new("/new"),
        );

        assert_eq!(repo.git_common_dir, PathBuf::from("/new/.git"));
        assert_eq!(repo.root_commits, vec![ROOT_A, ROOT_C]);
        assert_eq!(repo.worktrees[0].source_dir, PathBuf::from("/new"));
    }

    #[test]
    fn adopt_keeps_known_roots_against_a_commitless_resolve() {
        let mut repo = state(&[ROOT_A], "/repo/.git");
        repo.adopt(&identity(&[], "/repo/.git", ""), Path::new("/repo"));

        assert_eq!(repo.root_commits, vec![ROOT_A]);
    }

    #[test]
    fn git_runs_in_the_first_worktree_that_still_has_files() {
        let temp = TempRoot::new();
        let common = temp.0.join("repo.git");
        let files = temp.0.join("feature");
        fs::create_dir_all(common.join("worktrees/feature")).expect("creating the git dir");
        fs::create_dir_all(&files).expect("creating the worktree");
        let common = common.to_str().expect("a utf-8 temp path");

        // The main worktree is registered first, but its files are gone.
        let mut repo = RepoState::new(&identity(&[], common, ""), &temp.0.join("gone"));
        repo.adopt(&identity(&[], common, "worktrees/feature"), &files);

        assert_eq!(repo.scan_dir(), files);
    }

    #[test]
    fn git_runs_in_the_store_when_no_worktree_has_files() {
        let temp = TempRoot::new();
        let common = temp.0.join("repo.git");
        fs::create_dir_all(&common).expect("creating the store");
        let repo = RepoState::new(
            &identity(&[], common.to_str().expect("a utf-8 temp path"), ""),
            &temp.0.join("gone"),
        );

        assert_eq!(repo.scan_dir(), common);
    }

    #[test]
    fn a_store_showing_a_root_this_state_stored_is_ours() {
        let repo = state(&[ROOT_A], "/repo/.git");

        // An orphan branch has added a second root since.
        assert_eq!(
            repo.ownership(&identity(&[ROOT_A, ROOT_C], "/repo/.git", ""), false),
            Ownership::Ours
        );
    }

    #[test]
    fn a_store_whose_roots_miss_every_stored_one_holding_none_of_our_work_is_foreign() {
        // `rm -rf .git && git init`, or a different project cloned to the path.
        let repo = state(&[ROOT_A], "/repo/.git");

        assert_eq!(
            repo.ownership(&identity(&[ROOT_B], "/repo/.git", ""), false),
            Ownership::Foreign
        );
    }

    #[test]
    fn a_store_whose_roots_moved_but_still_holds_our_work_is_rerooted() {
        // `rebase --root`, an amend of the root commit, or `fetch --unshallow`.
        let repo = state(&[ROOT_A], "/repo/.git");

        assert_eq!(
            repo.ownership(&identity(&[ROOT_B], "/repo/.git", ""), true),
            Ownership::Rerooted
        );
    }

    #[test]
    fn a_store_with_no_roots_yet_is_neither() {
        // A clone in flight: `.git` is there, no ref is written yet. Answering
        // on the common dir would hand this state whatever lands.
        let repo = state(&[ROOT_A], "/repo/.git");

        assert_eq!(
            repo.ownership(&identity(&[], "/repo/.git", ""), false),
            Ownership::NotYet
        );
    }

    #[test]
    fn a_state_registered_commitless_owns_its_common_dir() {
        // Nothing was ever scored under it, so there is nothing to hand away.
        let repo = state(&[], "/repo/.git");

        assert_eq!(
            repo.ownership(&identity(&[], "/repo/.git", ""), false),
            Ownership::Ours
        );
        assert_eq!(
            repo.ownership(&identity(&[ROOT_A], "/repo/.git", ""), false),
            Ownership::Ours
        );
        assert_eq!(
            repo.ownership(&identity(&[], "/elsewhere/.git", ""), false),
            Ownership::Foreign
        );
    }

    #[test]
    fn events_past_the_cap_drop_from_the_front() {
        let mut repo = state(&[ROOT_A], "/repo/.git");

        // Two scans, so the cap is met across calls rather than within one.
        repo.record((0..150).map(event));
        repo.record((150..250).map(event));

        assert_eq!(repo.events.len(), MAX_EVENTS);
        assert_eq!(repo.events.first().unwrap().subject, "commit 50");
        assert_eq!(repo.events.last().unwrap().subject, "commit 249");
    }

    #[test]
    fn the_total_counts_the_events_the_cap_dropped() {
        let mut repo = state(&[ROOT_A], "/repo/.git");

        // 250 commits at 1 xp, of which the cap keeps 200.
        assert_eq!(repo.record((0..250).map(event)), 250);
        assert_eq!(repo.total_xp, 250);
        assert_eq!(repo.events.len(), MAX_EVENTS);
    }

    #[test]
    fn a_logs_dir_joins_the_common_dir() {
        let mut repo = state(&[ROOT_A], "/repo/.git");
        repo.adopt(
            &identity(&[ROOT_A], "/repo/.git", "worktrees/feature"),
            Path::new("/feature"),
        );

        assert_eq!(
            repo.head_dir(&repo.worktrees[0]),
            PathBuf::from("/repo/.git")
        );
        assert_eq!(
            repo.head_dir(&repo.worktrees[1]),
            PathBuf::from("/repo/.git/worktrees/feature")
        );
    }

    #[test]
    fn a_row_is_labelled_from_its_main_worktree() {
        let mut repo = RepoState::new(
            &identity(&[ROOT_A], "/repo/.git", "worktrees/f"),
            Path::new("/feature"),
        );
        assert_eq!(repo.label_dir(), Some(Path::new("/feature")));

        repo.adopt(&identity(&[ROOT_A], "/repo/.git", ""), Path::new("/repo"));
        assert_eq!(repo.label_dir(), Some(Path::new("/repo")));
    }
}
