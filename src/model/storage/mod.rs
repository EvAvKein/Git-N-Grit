//! Where state files live, and loading and saving them.

pub mod identity;
pub mod lock;
pub mod repo;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::model::git::RepoIdentity;
use repo::{Duplicate, RepoState};

const VERSION: u32 = 1;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    use super::repo::Event;

    pub(super) const ROOT_A: &str = "9e21c0f1a2b3c4d5e6f708192a3b4c5d6e7f8091";
    pub(super) const ROOT_B: &str = "8201f791a2b3c4d5e6f708192a3b4c5d6e7f8091";
    pub(super) const ROOT_C: &str = "d9324e91a2b3c4d5e6f708192a3b4c5d6e7f8091";

    /// A throwaway directory of our own, removed when the test ends.
    pub(super) struct TempRoot(pub(super) PathBuf);

    impl TempRoot {
        pub(super) fn new() -> Self {
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

    pub(super) fn identity(roots: &[&str], common: &str, head_subdir: &str) -> RepoIdentity {
        RepoIdentity {
            root_commits: roots.iter().map(|s| s.to_string()).collect(),
            git_common_dir: PathBuf::from(common),
            head_subdir: PathBuf::from(head_subdir),
        }
    }

    pub(super) fn state(roots: &[&str], common: &str) -> RepoState {
        RepoState::new(&identity(roots, common, ""), Path::new("/repo"))
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

    /// A shared store's two files, by id, marked `(Scoring, Shadowed)` in that
    /// order.
    fn scoring_and_shadowed(loaded: &Loaded) -> (Uuid, Uuid) {
        let scoring = loaded
            .repos
            .iter()
            .find(|r| r.duplicate == Duplicate::Scoring)
            .expect("one scoring")
            .id;
        let shadowed = loaded
            .repos
            .iter()
            .find(|r| r.duplicate == Duplicate::Shadowed)
            .expect("one shadowed")
            .id;
        (scoring, shadowed)
    }

    #[test]
    fn removing_the_shadowed_copy_leaves_the_scorer_undisputed() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        storage
            .save(&state(&[ROOT_A], "/repo/.git"))
            .expect("saving");
        storage
            .save(&state(&[ROOT_A], "/repo/.git"))
            .expect("saving");
        let (scoring, shadowed) = scoring_and_shadowed(&storage.load_all());

        storage
            .remove(shadowed)
            .expect("removing the shadowed copy");

        let loaded = storage.load_all();
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].id, scoring);
        assert_eq!(loaded.repos[0].duplicate, Duplicate::None);
    }

    #[test]
    fn removing_the_scoring_copy_promotes_the_shadowed_one() {
        let temp = TempRoot::new();
        let storage = temp.storage();
        storage
            .save(&state(&[ROOT_A], "/repo/.git"))
            .expect("saving");
        storage
            .save(&state(&[ROOT_A], "/repo/.git"))
            .expect("saving");
        let (scoring, shadowed) = scoring_and_shadowed(&storage.load_all());

        storage.remove(scoring).expect("removing the scoring copy");

        let loaded = storage.load_all();
        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos[0].id, shadowed);
        assert_eq!(loaded.repos[0].duplicate, Duplicate::None);
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
}
