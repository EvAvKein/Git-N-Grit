//! Throwaway git repos for the integration tests.

// Each test binary uses its own subset.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use gng::model::git::resolve_identity;
use gng::model::scan::{ScanFindings, apply, run_scan, run_verify};
use gng::model::storage::Storage;
use gng::model::storage::identity::{Match, match_identity, share_a_root_commit};
use gng::model::storage::repo::RepoState;
use uuid::Uuid;

/// The identity every test commits under, and what each repo's local
/// `user.email` is set to.
pub const MINE: &str = "eve@example.com";
/// Somebody else on a shared repo.
pub const THEIRS: &str = "someone@else.example";

/// A temp directory holding the repos of one test, and its storage root.
pub struct Fixture {
    root: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        let root = std::env::temp_dir().join(format!("gng-it-{}", Uuid::new_v4()));
        // Empty, and every repo's core.hooksPath, so a hook in a real
        // ~/.gitconfig cannot fail a commit here.
        fs::create_dir_all(root.join("hooks")).expect("creating a temp dir");
        Fixture { root }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// `git init` a repo, configured for the library's git calls to read.
    pub fn repo(&self, name: &str) -> Repo {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).expect("creating the repo dir");
        let repo = self.attach(dir);
        repo.run(&["init", "-b", "main"]);
        repo.isolate();
        repo
    }

    /// A handle on a directory that is already a repo — a linked worktree, or
    /// a clone.
    pub fn attach(&self, dir: PathBuf) -> Repo {
        Repo {
            dir,
            hooks: self.root.join("hooks"),
        }
    }

    /// `git init --separate-git-dir`: the working tree and the store it
    /// points at live in different directories, isolated the same way
    /// `repo()` is.
    pub fn separate_git_dir_repo(&self, name: &str, git_dir_name: &str) -> Repo {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).expect("creating the repo dir");
        let repo = self.attach(dir);
        let git_dir = self.root.join(git_dir_name);
        repo.run(&[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            git_dir.to_str().expect("a utf-8 temp path"),
        ]);
        repo.isolate();
        repo
    }

    /// `git clone` from `source`, isolated the same way `repo()` is.
    pub fn clone_of(&self, source: &Repo, name: &str) -> Repo {
        let dir = self.root.join(name);
        let out = Command::new("git")
            .arg("clone")
            .arg(&source.dir)
            .arg(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("cloning");
        assert!(
            out.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let repo = self.attach(dir);
        repo.isolate();
        repo
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub struct Repo {
    pub dir: PathBuf,
    hooks: PathBuf,
}

impl Repo {
    /// The isolation the *library's* git calls read.
    ///
    /// It has to be repo-local config rather than the environment: those calls
    /// are spawned by `gng` itself and inherit the test binary's environment,
    /// and `std::env::set_var` is `unsafe` in edition 2024 and races every
    /// other test in the binary. Local config beats global, which is what
    /// keeps a real `~/.gitconfig` out of a throwaway repo.
    fn isolate(&self) {
        let hooks = self.hooks.display().to_string();
        self.run(&["config", "user.name", "Eve"]);
        self.run(&["config", "user.email", MINE]);
        self.run(&["config", "commit.gpgsign", "false"]);
        self.run(&["config", "log.showSignature", "false"]);
        self.run(&["config", "core.hooksPath", &hooks]);
    }

    /// A git call the test itself makes, which *can* be given an environment.
    pub fn run(&self, args: &[&str]) -> String {
        self.run_as(MINE, args)
    }

    /// The same, authored by somebody else.
    pub fn run_as(&self, author: &str, args: &[&str]) -> String {
        let mut cmd = self.command(args);
        cmd.env("GIT_AUTHOR_EMAIL", author);
        Self::output(args, &mut cmd)
    }

    /// The same, authored and committed at `at`, in Unix seconds.
    pub fn run_at(&self, author: &str, at: i64, args: &[&str]) -> String {
        let date = format!("@{at} +0000");
        let mut cmd = self.command(args);
        cmd.env("GIT_AUTHOR_EMAIL", author)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
        Self::output(args, &mut cmd)
    }

    fn output(args: &[&str], cmd: &mut Command) -> String {
        let out = cmd.output().expect("running git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Eve")
            .env("GIT_AUTHOR_EMAIL", MINE)
            .env("GIT_COMMITTER_NAME", "Eve")
            .env("GIT_COMMITTER_EMAIL", MINE);
        cmd
    }

    pub fn write(&self, name: &str, contents: &str) {
        fs::write(self.dir.join(name), contents).expect("writing a file");
    }

    /// NUL bytes are what make git call a file binary.
    pub fn write_bytes(&self, name: &str, bytes: &[u8]) {
        fs::write(self.dir.join(name), bytes).expect("writing a file");
    }

    /// Stage everything and commit.
    pub fn commit(&self, message: &str) -> String {
        self.commit_as(MINE, message)
    }

    pub fn commit_as(&self, author: &str, message: &str) -> String {
        self.run(&["add", "-A"]);
        self.run_as(author, &["commit", "-m", message]);
        self.head()
    }

    /// One commit writing `contents` to `name`.
    pub fn commit_file(&self, name: &str, contents: &str, message: &str) -> String {
        self.write(name, contents);
        self.commit(message)
    }

    pub fn head(&self) -> String {
        self.run(&["rev-parse", "HEAD"])
    }

    /// This worktree's own `logs/HEAD`.
    pub fn reflog(&self) -> PathBuf {
        let git_dir = self.run(&["rev-parse", "--path-format=absolute", "--git-dir"]);
        PathBuf::from(git_dir).join("logs/HEAD")
    }

    /// Register this repo as the app's add prompt would.
    pub fn state(&self) -> RepoState {
        RepoState::new(&self.identity(), &self.dir)
    }

    pub fn identity(&self) -> gng::model::git::RepoIdentity {
        resolve_identity(&self.dir).expect("resolving the repo")
    }
}

/// Scan and bank it, as the app will. Returns the XP earned.
pub fn scan(state: &mut RepoState) -> u64 {
    let findings = run_scan(state).expect("a scan");
    apply(state, findings)
}

/// The findings alone, for the cases that assert on what a scan did *not* do.
pub fn findings(state: &RepoState) -> ScanFindings {
    run_scan(state).expect("a scan")
}

/// The findings of a scan asked to resolve the identity whatever it finds.
pub fn verify(state: &RepoState) -> ScanFindings {
    run_verify(state).expect("a verifying scan")
}

/// A scan that ended at step 2 never resolved an identity, which is the mark
/// of one that reached git not at all.
pub fn reached_git(findings: &ScanFindings) -> bool {
    findings.ownership.is_some()
}

/// What resolving an add would produce. Mirrors `Model::resolve_add`, which
/// integration tests cannot reach directly since it is crate-private — this
/// replicates its decision against `Storage` and `state::match_identity` alone.
pub enum Added {
    Registered(RepoState),
    Pick {
        candidates: Vec<RepoState>,
        offer_new: bool,
    },
}

/// Resolve `repo`'s identity against what is already registered, as the add
/// prompt would.
pub fn add(storage: &Storage, repo: &Repo) -> Added {
    let identity = repo.identity();
    let loaded = storage.load_all();
    assert!(
        loaded.errors.is_empty(),
        "bad state files: {:?}",
        loaded.errors
    );

    match match_identity(&loaded.repos, &identity, |p: &Path| p.exists()) {
        Match::Reuse(id) => {
            let mut state = loaded
                .repos
                .into_iter()
                .find(|s| s.id == id)
                .expect("match_identity named a state that was loaded");
            storage
                .adopt(&mut state, &identity, &repo.dir)
                .expect("adopting");
            Added::Registered(state)
        }
        Match::New => Added::Registered(register_new(storage, repo)),
        Match::Selection(ids) => {
            let store = &identity.git_common_dir;
            let already_registered = loaded.repos.iter().any(|s| {
                ids.contains(&s.id)
                    && &s.git_common_dir == store
                    && share_a_root_commit(&s.root_commits, &identity.root_commits)
            });
            let candidates = loaded
                .repos
                .into_iter()
                .filter(|s| ids.contains(&s.id))
                .collect();
            Added::Pick {
                candidates,
                offer_new: !already_registered,
            }
        }
    }
}

/// A first-time add, and the "new repo" pick at the picker, both take this path.
pub fn register_new(storage: &Storage, repo: &Repo) -> RepoState {
    let state = RepoState::new(&repo.identity(), &repo.dir);
    storage.save(&state).expect("saving");
    state
}

/// Taking an existing row at the picker.
pub fn register_into(storage: &Storage, mut state: RepoState, repo: &Repo) -> RepoState {
    storage
        .adopt(&mut state, &repo.identity(), &repo.dir)
        .expect("adopting");
    state
}
