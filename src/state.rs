//! One state file per object store, and the identity match that locates one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::git::RepoIdentity;

/// Newest events kept per repo. Display-only: `total_xp` is its own running
/// sum, so dropping the rest costs nothing.
pub const MAX_EVENTS: usize = 200;

const VERSION: u32 = 1;

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

/// What an add resolves to against the state files already on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    /// Exactly one store is this one. Adopt into it.
    Reuse(Uuid),
    /// Ambiguous: the user picks one, or "new repo".
    Selection(Vec<Uuid>),
    /// Nothing on disk relates to it.
    New,
}

/// Which state file, if any, the resolved identity belongs to.
///
/// `is_live` answers whether a stored common dir is still on disk. It is a
/// parameter rather than a `Path::exists` call so the rules test without a
/// disk, and so the scan worker can publish each path it is about to stat —
/// any one of these candidates can be the one that hangs.
/// Whether two histories share a root commit.
pub fn share_a_root_commit(a: &[String], b: &[String]) -> bool {
    a.iter().any(|c| b.contains(c))
}

pub fn match_identity(
    states: &[RepoState],
    identity: &RepoIdentity,
    is_live: impl Fn(&Path) -> bool,
) -> Match {
    let same_store = |s: &RepoState| s.git_common_dir == identity.git_common_dir;

    let candidates: Vec<Uuid> = states
        .iter()
        .filter(|s| {
            // No roots on either side leaves nothing to intersect, so the
            // common dir answers alone — a repo registered before its first
            // commit, or re-added before the scan that adopts one.
            let related = if s.root_commits.is_empty() || identity.root_commits.is_empty() {
                same_store(s)
            } else {
                share_a_root_commit(&s.root_commits, &identity.root_commits)
            };
            // A store that has moved is still that store; one that hasn't, and
            // sits elsewhere, is a different clone of the same upstream.
            related && (same_store(s) || !is_live(&s.git_common_dir))
        })
        .map(|s| s.id)
        .collect();

    match candidates.len() {
        1 => Match::Reuse(candidates[0]),
        0 => {
            // Unrelated history over a git dir already registered: a repo
            // re-initialised in place, or a different project cloned over a
            // deleted one. Git cannot tell those apart, so the user does.
            let occupants: Vec<Uuid> = states
                .iter()
                .filter(|s| same_store(s) && is_live(&s.git_common_dir))
                .map(|s| s.id)
                .collect();
            if occupants.is_empty() {
                Match::New
            } else {
                Match::Selection(occupants)
            }
        }
        _ => Match::Selection(candidates),
    }
}

/// The storage root and every path under it. Built once in `main` from
/// `--storage` or `dirs::data_dir()`, then handed down.
#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

/// The result of a startup load: one bad state file costs its own repo only.
#[derive(Debug, Default)]
pub struct Loaded {
    pub repos: Vec<RepoState>,
    pub errors: Vec<LoadError>,
}

/// A state file that didn't load. The path is what the row labels itself from
/// and what removing it deletes, so it stays a path rather than a message.
///
/// A path equal to `Storage::repos_dir` is the directory itself failing to
/// read, which means nothing under it loaded. That row names the directory and
/// offers no removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

impl Storage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Storage { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    /// The exclusive lock, held for the process's lifetime.
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(".take")
    }

    /// Exists only while a second instance is asking for the lock.
    pub fn takeover_path(&self) -> PathBuf {
        self.root.join(".takeover")
    }

    fn state_path(&self, id: Uuid) -> PathBuf {
        self.repos_dir().join(format!("{id}.json"))
    }

    /// Every state file, in the left pane's order. Matching `*.json` rather
    /// than `*` is what makes a `.tmp` left by an interrupted write harmless.
    pub fn load_all(&self) -> Loaded {
        let mut loaded = Loaded::default();
        // Absent where the file read but its mtime would not, which is what
        // leaves a shared store `Undecided`.
        let mut written: HashMap<Uuid, SystemTime> = HashMap::new();

        let entries = match fs::read_dir(self.repos_dir()) {
            Ok(entries) => entries,
            // Nothing registered yet is not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return loaded,
            Err(e) => {
                loaded.errors.push(LoadError {
                    path: self.repos_dir(),
                    message: e.to_string(),
                });
                return loaded;
            }
        };

        for entry in entries {
            // An entry that won't read names no file, so it answers for the
            // directory. Dropping it would lose a state file in silence.
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    loaded.errors.push(LoadError {
                        path: self.repos_dir(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match load_one(&path) {
                Ok(state) => {
                    if let Ok(at) = fs::metadata(&path).and_then(|m| m.modified()) {
                        written.insert(state.id, at);
                    }
                    loaded.repos.push(state);
                }
                Err(e) => loaded.errors.push(LoadError {
                    path,
                    message: format!("{e:#}"),
                }),
            }
        }

        loaded
            .repos
            .sort_by(|a, b| (a.added_at, &a.git_common_dir).cmp(&(b.added_at, &b.git_common_dir)));
        mark_duplicates(&mut loaded.repos, &written);
        loaded
    }

    /// Write to a temp file beside the target, then rename over it. In-place
    /// would truncate first, and a crash there leaves unparseable JSON where a
    /// stale but valid state file had been — a lost `seen_patch_ids`.
    pub fn save(&self, state: &RepoState) -> Result<()> {
        let dir = self.repos_dir();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let mut json = serde_json::to_vec_pretty(state).context("serializing repo state")?;
        json.push(b'\n');

        // Same directory, so the rename is within one filesystem and atomic.
        let tmp = dir.join(format!("{}.json.tmp", state.id));
        fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, self.state_path(state.id))
            .with_context(|| format!("renaming {}", tmp.display()))
    }

    /// `adopt` and persist, which is every reuse path.
    pub fn adopt(
        &self,
        state: &mut RepoState,
        identity: &RepoIdentity,
        source_dir: &Path,
    ) -> Result<()> {
        state.adopt(identity, source_dir);
        self.save(state)
    }

    pub fn remove(&self, id: Uuid) -> Result<()> {
        let path = self.state_path(id);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
    }
}

/// Mark the states that share an object store, and pick the one that earns.
///
/// `written` holds each file's mtime, and a state missing from it is one whose
/// mtime would not read. Where a shared store has such a file, no file on that
/// store scores: the newest cannot be named, and the rest of the group might
/// each be it.
fn mark_duplicates(repos: &mut [RepoState], written: &HashMap<Uuid, SystemTime>) {
    let mut by_store: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, repo) in repos.iter().enumerate() {
        by_store
            .entry(repo.git_common_dir.clone())
            .or_default()
            .push(i);
    }

    for group in by_store.into_values().filter(|g| g.len() > 1) {
        let Some(mtimes) = group
            .iter()
            .map(|&i| written.get(&repos[i].id).map(|at| (at, i)))
            .collect::<Option<Vec<_>>>()
        else {
            for i in group {
                repos[i].duplicate = Duplicate::Undecided;
            }
            continue;
        };

        // Two files written inside one mtime tick are a tie rather than a
        // failure, and the id settles it the same way at every start.
        let scoring = mtimes
            .into_iter()
            .max_by_key(|&(at, i)| (at, repos[i].id))
            .map(|(_, i)| i)
            .expect("a group holds more than one");

        for i in group {
            repos[i].duplicate = if i == scoring {
                Duplicate::Scoring
            } else {
                Duplicate::Shadowed
            };
        }
    }
}

fn load_one(path: &Path) -> Result<RepoState> {
    let text = fs::read_to_string(path)?;
    let mut state: RepoState = serde_json::from_str(&text)?;
    // A newer writer's fields are dropped by serde, and saving would write the
    // truncation back. Skip the repo instead.
    if state.file_format_version > VERSION {
        bail!(
            "version {} is newer than this build's {VERSION}",
            state.file_format_version
        );
    }
    // Skipped by serde, so it comes from the filename or from nowhere.
    state.id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .context("filename is not a UUID")?;
    Ok(state)
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
    use std::time::Duration;

    const ROOT_A: &str = "9e21c0f1a2b3c4d5e6f708192a3b4c5d6e7f8091";
    const ROOT_B: &str = "8201f791a2b3c4d5e6f708192a3b4c5d6e7f8091";
    const ROOT_C: &str = "d9324e91a2b3c4d5e6f708192a3b4c5d6e7f8091";

    /// A throwaway directory of our own, removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gng-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("creating a temp dir");
            TempRoot(path)
        }

        fn storage(&self) -> Storage {
            Storage::new(&self.0)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn identity(roots: &[&str], common: &str, head_subdir: &str) -> RepoIdentity {
        RepoIdentity {
            root_commits: roots.iter().map(|s| s.to_string()).collect(),
            git_common_dir: PathBuf::from(common),
            head_subdir: PathBuf::from(head_subdir),
        }
    }

    fn state(roots: &[&str], common: &str) -> RepoState {
        RepoState::new(&identity(roots, common, ""), Path::new("/repo"))
    }

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

    /// Every path is live unless it is named here.
    fn live_except(gone: &[&str]) -> impl Fn(&Path) -> bool + use<> {
        let gone: Vec<PathBuf> = gone.iter().map(PathBuf::from).collect();
        move |p: &Path| !gone.contains(&p.to_path_buf())
    }

    #[test]
    fn one_intersecting_root_reuses_that_state() {
        let existing = state(&[ROOT_A], "/repo/.git");
        let id = existing.id;
        // An orphan branch has added a second root since.
        let resolved = identity(&[ROOT_A, ROOT_C], "/repo/.git", "");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&[])),
            Match::Reuse(id)
        );
    }

    #[test]
    fn a_moved_repo_reuses_its_state() {
        let existing = state(&[ROOT_A], "/old/.git");
        let id = existing.id;
        let resolved = identity(&[ROOT_A], "/new/.git", "");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&["/old/.git"])),
            Match::Reuse(id)
        );
    }

    #[test]
    fn a_fork_beside_its_parent_is_a_new_repo() {
        // Same root commits, but the parent's common dir is still there.
        let parent = state(&[ROOT_A], "/parent/.git");
        let resolved = identity(&[ROOT_A], "/fork/.git", "");

        assert_eq!(
            match_identity(&[parent], &resolved, live_except(&[])),
            Match::New
        );
    }

    #[test]
    fn two_moved_clones_of_one_upstream_go_to_the_picker() {
        let one = state(&[ROOT_A], "/gone/one/.git");
        let two = state(&[ROOT_A], "/gone/two/.git");
        let ids = vec![one.id, two.id];
        let resolved = identity(&[ROOT_A], "/here/.git", "");

        assert_eq!(
            match_identity(
                &[one, two],
                &resolved,
                live_except(&["/gone/one/.git", "/gone/two/.git"])
            ),
            Match::Selection(ids)
        );
    }

    #[test]
    fn unrelated_history_over_a_registered_git_dir_goes_to_the_picker() {
        // `rm -rf .git && git init`, or a different project cloned to the path.
        let existing = state(&[ROOT_A], "/repo/.git");
        let id = existing.id;
        let resolved = identity(&[ROOT_B], "/repo/.git", "");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&[])),
            Match::Selection(vec![id])
        );
    }

    #[test]
    fn every_state_on_a_registered_git_dir_reaches_the_picker() {
        // Two rows come to share a git dir when a state file is copied, or
        // when one is adopted onto a path another already holds. A third
        // history over it has to be able to land on either.
        let first = state(&[ROOT_A], "/repo/.git");
        let second = state(&[ROOT_B], "/repo/.git");
        let ids = vec![first.id, second.id];
        let resolved = identity(&[ROOT_C], "/repo/.git", "");

        assert_eq!(
            match_identity(&[first, second], &resolved, live_except(&[])),
            Match::Selection(ids)
        );
    }

    #[test]
    fn an_unrelated_repo_elsewhere_is_new() {
        let existing = state(&[ROOT_A], "/one/.git");
        let resolved = identity(&[ROOT_B], "/two/.git", "");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&[])),
            Match::New
        );
    }

    #[test]
    fn a_linked_worktree_matches_its_repo() {
        let existing = state(&[ROOT_A], "/repo/.git");
        let id = existing.id;
        let resolved = identity(&[ROOT_A], "/repo/.git", "worktrees/feature");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&[])),
            Match::Reuse(id)
        );
    }

    #[test]
    fn a_commitless_repo_matches_on_its_common_dir_alone() {
        let existing = state(&[], "/repo/.git");
        let id = existing.id;
        let resolved = identity(&[], "/repo/.git", "");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&[])),
            Match::Reuse(id)
        );
    }

    #[test]
    fn a_repo_that_has_gained_its_first_commit_still_matches() {
        // Registered commitless, re-added before the scan that adopts a root.
        let existing = state(&[], "/repo/.git");
        let id = existing.id;
        let resolved = identity(&[ROOT_A], "/repo/.git", "");

        assert_eq!(
            match_identity(&[existing], &resolved, live_except(&[])),
            Match::Reuse(id)
        );
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
    fn a_saved_state_loads_back_identical() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let mut repo = state(&[ROOT_A], "/repo/.git");
        repo.total_xp = 1240;
        repo.awarded.insert("a0f0365".into(), 7);
        repo.seen_patch_ids.insert("84b8408".into());
        repo.events.push(Event {
            sha: "a0f0365".into(),
            timestamp: 1756100000,
            subject: "feat: parse reflog".into(),
            added: 120,
            deleted: 34,
            xp: 222,
            rewrite: false,
        });

        storage.save(&repo).expect("saving");

        assert_eq!(storage.load_all().repos, vec![repo]);
    }

    #[test]
    fn repos_load_in_added_order() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let mut first = state(&[ROOT_A], "/one/.git");
        let mut second = state(&[ROOT_B], "/two/.git");
        first.added_at = 100;
        second.added_at = 200;

        storage.save(&second).expect("saving");
        storage.save(&first).expect("saving");

        let ids: Vec<Uuid> = storage.load_all().repos.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[test]
    fn a_stray_tmp_file_is_ignored() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let repo = state(&[ROOT_A], "/repo/.git");
        storage.save(&repo).expect("saving");
        fs::write(storage.repos_dir().join("half-written.json.tmp"), "{").expect("writing");

        let loaded = storage.load_all();
        assert_eq!(loaded.repos.len(), 1);
        assert!(loaded.errors.is_empty());
    }

    #[test]
    fn an_unparseable_state_file_skips_only_itself() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let good = state(&[ROOT_A], "/repo/.git");
        storage.save(&good).expect("saving");
        let bad = storage.repos_dir().join(format!("{}.json", Uuid::new_v4()));
        fs::write(&bad, "{").expect("writing");

        let loaded = storage.load_all();
        assert_eq!(loaded.repos, vec![good]);
        assert_eq!(loaded.errors.len(), 1);
        // The row that renders this labels itself from the path, and removing
        // it deletes that file.
        assert_eq!(loaded.errors[0].path, bad);
    }

    #[test]
    fn a_newer_version_skips_only_its_own_repo() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let good = state(&[ROOT_A], "/repo/.git");
        storage.save(&good).expect("saving");

        let mut newer = state(&[ROOT_B], "/newer/.git");
        newer.file_format_version = VERSION + 1;
        storage.save(&newer).expect("saving");

        let loaded = storage.load_all();
        assert_eq!(loaded.repos, vec![good]);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(
            loaded.errors[0].path,
            storage.repos_dir().join(format!("{}.json", newer.id))
        );
    }

    /// The tripwire on `awarded` carrying no `#[serde(default)]`. With one, a
    /// file like this would load holding its total while the record of what
    /// earned it was empty, and the next scan would find the whole reflog
    /// unscored and bank every commit again at full value. It claims this
    /// build's version, so the missing field is the only thing that can reject
    /// it.
    #[test]
    fn a_state_file_with_no_awarded_does_not_load() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let good = state(&[ROOT_A], "/repo/.git");
        storage.save(&good).expect("saving");

        let without = storage.repos_dir().join(format!("{}.json", Uuid::new_v4()));
        fs::write(
            &without,
            format!(
                r#"{{"file_format_version":{VERSION},"root_commits":["{ROOT_B}"],
                     "git_common_dir":"/other/.git","added_at":100,"total_xp":1240,
                     "seen_patch_ids":[],"worktrees":[],"events":[]}}"#
            ),
        )
        .expect("writing");

        let loaded = storage.load_all();
        assert_eq!(loaded.repos, vec![good]);
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].path, without);
    }

    #[test]
    fn removing_a_repo_leaves_the_others() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let one = state(&[ROOT_A], "/one/.git");
        let two = state(&[ROOT_B], "/two/.git");
        storage.save(&one).expect("saving");
        storage.save(&two).expect("saving");

        storage.remove(one.id).expect("removing");

        assert_eq!(storage.load_all().repos, vec![two]);
    }

    /// Every state's mtime, `n` seconds apart in the order given.
    fn written_seconds_apart(repos: &[RepoState]) -> HashMap<Uuid, SystemTime> {
        repos
            .iter()
            .enumerate()
            .map(|(n, r)| (r.id, UNIX_EPOCH + Duration::from_secs(n as u64)))
            .collect()
    }

    fn duplicates(repos: &[RepoState]) -> Vec<Duplicate> {
        repos.iter().map(|r| r.duplicate).collect()
    }

    #[test]
    fn the_newest_file_on_a_shared_store_is_the_one_that_scores() {
        let mut repos = vec![
            state(&[ROOT_A], "/repo/.git"),
            state(&[ROOT_A], "/repo/.git"),
        ];
        let written = written_seconds_apart(&repos);

        mark_duplicates(&mut repos, &written);

        assert_eq!(
            duplicates(&repos),
            vec![Duplicate::Shadowed, Duplicate::Scoring]
        );
    }

    #[test]
    fn a_store_named_once_is_not_a_duplicate() {
        let mut repos = vec![state(&[ROOT_A], "/one/.git"), state(&[ROOT_B], "/two/.git")];
        let written = written_seconds_apart(&repos);

        mark_duplicates(&mut repos, &written);

        assert_eq!(duplicates(&repos), vec![Duplicate::None, Duplicate::None]);
    }

    #[test]
    fn an_unreadable_mtime_leaves_the_whole_store_undecided() {
        let mut repos = vec![
            state(&[ROOT_A], "/repo/.git"),
            state(&[ROOT_A], "/repo/.git"),
        ];
        // The second file read, but would not say when it was written.
        let mut written = written_seconds_apart(&repos);
        written.remove(&repos[1].id);

        mark_duplicates(&mut repos, &written);

        assert_eq!(
            duplicates(&repos),
            vec![Duplicate::Undecided, Duplicate::Undecided]
        );
    }

    #[test]
    fn a_shared_store_is_marked_across_a_save_and_load() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        let first = state(&[ROOT_A], "/repo/.git");
        let second = state(&[ROOT_A], "/repo/.git");
        storage.save(&first).expect("saving");
        storage.save(&second).expect("saving");

        let loaded = storage.load_all();

        assert_eq!(loaded.repos.len(), 2);
        assert_eq!(
            loaded
                .repos
                .iter()
                .filter(|r| r.duplicate == Duplicate::Scoring)
                .count(),
            1
        );
        assert!(loaded.repos.iter().all(|r| r.duplicate != Duplicate::None));
    }

    #[test]
    fn a_repos_dir_that_will_not_read_is_one_error_naming_it() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        // A file where the directory belongs, so the read fails outright.
        fs::write(storage.repos_dir(), "").expect("writing");

        let loaded = storage.load_all();

        assert!(loaded.repos.is_empty());
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].path, storage.repos_dir());
    }

    #[test]
    fn an_empty_storage_root_loads_nothing() {
        let temp = TempRoot::new();

        let loaded = temp.storage().load_all();
        assert!(loaded.repos.is_empty());
        assert!(loaded.errors.is_empty());
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
