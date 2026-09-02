//! Throwaway git repos for the integration tests.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use gng::git::resolve_identity;
use gng::scan::{ScanDelta, apply, run_scan};
use gng::state::RepoState;
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
        let out = self
            .command(args)
            .env("GIT_AUTHOR_EMAIL", author)
            .output()
            .expect("running git");

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

    pub fn identity(&self) -> gng::git::RepoIdentity {
        resolve_identity(&self.dir).expect("resolving the repo")
    }
}

/// Scan and bank it, as the app will. Returns the XP earned.
pub fn scan(state: &mut RepoState) -> u64 {
    let delta = run_scan(state).expect("a scan");
    apply(state, delta)
}

/// The delta alone, for the cases that assert on what a scan did *not* do.
pub fn delta(state: &RepoState) -> ScanDelta {
    run_scan(state).expect("a scan")
}

/// A scan that ended at step 2 never resolved an identity, which is the mark
/// of one that reached git not at all.
pub fn reached_git(delta: &ScanDelta) -> bool {
    delta.ownership.is_some()
}
