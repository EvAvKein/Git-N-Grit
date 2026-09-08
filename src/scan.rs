//! The three steps, the scoring, and the one thread that runs them.
//!
//! Every `stat` and every `git` the app spawns happens here. A path that has
//! stopped answering keeps whichever thread touched it, and the main thread is
//! the one that has to go on drawing and answering `q`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::event::AppEvent;
use crate::git::{self, CommitInfo, RepoIdentity};
use crate::probe::InFlight;
use crate::reflog;
use crate::state::{Event, Match, Ownership, RepoState, match_identity};

/// What a rewrite of work already paid for earns. The work counts once; the
/// cleanup still counts a little.
const REWRITE_XP: u64 = 2;

/// Deleting code is worth more than writing it.
const DELETED_WEIGHT: u64 = 3;

/// Work posted to the worker.
pub enum Job {
    /// Scan this store. The whole state is cloned in, so the worker shares
    /// nothing with the main thread and needs nothing back to do its job.
    Scan(Box<RepoState>),
    /// Resolve a path typed at the add prompt, against what is registered.
    Resolve {
        /// The prompt's generation. An answer to a prompt that has moved on is
        /// dropped rather than acted on.
        seq: u64,
        path: PathBuf,
        states: Vec<RepoState>,
    },
}

/// One SHA the scan ran through scoring, and the XP it was awarded — zero for a
/// merge, another author's commit, or an object gc pruned.
#[derive(Debug, PartialEq, Eq)]
pub struct Awarded {
    pub sha: String,
    pub xp: u64,
}

/// What one scan found. Applying it is [`apply`]; the markers it feeds — the
/// `!`, the watch table — are the main thread's.
#[derive(Debug, Default)]
pub struct ScanDelta {
    /// In `ct` order, as scored, so appending keeps storage chronological.
    pub events: Vec<Event>,
    /// Each SHA scored this pass, zero-XP entries included.
    pub new_awarded: Vec<Awarded>,
    pub new_patch_ids: Vec<String>,
    /// Set only where the state held none and git resolved some.
    pub adopted_roots: Option<Vec<String>>,
    /// `None` where the scan ended before it could ask, which is most scans.
    /// The previous answer stands, or the `!` would blink off a repo that
    /// still cannot earn.
    pub no_email: Option<bool>,
    /// `None` where the scan had no cause to resolve it: stale, or nothing
    /// left to score.
    pub ownership: Option<Ownership>,
    /// The object store is gone. Keeps its total, earns nothing, is not
    /// watched.
    pub stale: bool,
    /// Directories to hand `notify` — each live worktree's `logs`, or its git
    /// dir where no `logs/` exists yet. Resolving that is a `stat`, so it is
    /// answered here rather than on the main thread.
    pub watch: Vec<PathBuf>,
}

#[derive(Default)]
struct Shared {
    in_flight: InFlight,
    stop: AtomicBool,
}

/// A thread running scans and resolves, one at a time.
pub struct Worker {
    shared: Arc<Shared>,
}

impl Worker {
    /// Spawn the worker and hand back the queue to post jobs on. The
    /// `JoinHandle` is dropped deliberately: a `git` call on a dead mount must
    /// not be something the app can wait on.
    pub fn spawn(tx: Sender<AppEvent>) -> (Worker, Sender<Job>) {
        let (jobs_tx, jobs_rx) = channel();
        let shared = Arc::new(Shared::default());
        {
            let shared = Arc::clone(&shared);
            thread::spawn(move || run(&shared, &jobs_rx, &tx));
        }
        (Worker { shared }, jobs_tx)
    }

    /// The path one call has been sitting in for longer than `limit`. The
    /// answer is to [`abandon`](Worker::abandon) this worker and spawn one
    /// whose jobs leave that store out.
    pub fn wedged(&self, limit: Duration) -> Option<PathBuf> {
        self.shared.in_flight.wedged(limit)
    }

    /// Stop, without waiting. Dropping the job sender alongside this is what
    /// unblocks a worker waiting on an empty queue; the flag is for one
    /// between jobs. A wedged thread sees neither and is simply outlived,
    /// taking the jobs it never answered with it — the app posts those again.
    pub fn abandon(self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn run(shared: &Shared, jobs: &Receiver<Job>, tx: &Sender<AppEvent>) {
    while let Ok(job) = jobs.recv() {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }

        let event = match job {
            Job::Scan(state) => AppEvent::ScanDone(state.id, scan(&state, &shared.in_flight)),
            Job::Resolve { seq, path, states } => AppEvent::Resolved {
                seq,
                result: resolve(&path, &states, &shared.in_flight),
            },
        };

        // A dropped receiver is the app going away.
        if tx.send(event).is_err() {
            return;
        }
    }
}

/// One scan of one store. Public so tests drive it with no thread, channel or
/// timing in the way.
pub fn run_scan(state: &RepoState) -> Result<ScanDelta> {
    scan(state, &InFlight::default())
}

/// Resolve a typed path and place it against what is registered.
pub fn run_resolve(path: &Path, states: &[RepoState]) -> Result<(RepoIdentity, Match)> {
    resolve(path, states, &InFlight::default())
}

fn scan(state: &RepoState, in_flight: &InFlight) -> Result<ScanDelta> {
    let mut delta = ScanDelta::default();

    // One bracket for every path read here — all under the store's dir, and
    // the app abandons wedged work per store.
    let Some(store) = watching(in_flight, &state.git_common_dir, || {
        (!state.is_stale()).then(|| read_store(state))
    }) else {
        delta.stale = true;
        return Ok(delta);
    };
    delta.watch = store.watch;

    // What the reflogs name that has not already been processed. Cost from
    // here scales with what is left to score, not with the reflogs' size.
    let named = reflog::extract_shas(&store.reflogs);
    let to_score: Vec<String> = named
        .iter()
        .filter(|sha| !state.awarded.contains_key(*sha))
        .cloned()
        .collect();
    if to_score.is_empty() {
        return Ok(delta); // git is not reached at all
    }
    // Anything subtracted was scored into this state before this store could
    // have been created, so the store is the one we have been reading.
    let holds_seen_commits = to_score.len() < named.len();

    // A path can be refilled by an unrelated project between one scan and the
    // next, and that store is live, watched, and full of commits.
    let dir = store.scan_dir;
    let identity = watching(in_flight, &dir, || git::resolve_identity(&dir))?;
    let ownership = state.ownership(&identity, holds_seen_commits);
    delta.ownership = Some(ownership);
    if ownership != Ownership::Ours {
        return Ok(delta);
    }
    if state.root_commits.is_empty() && !identity.root_commits.is_empty() {
        delta.adopted_roots = Some(identity.root_commits);
    }

    // Read per scan and never stored, so changing your identity takes effect
    // on the next scan with nothing to migrate.
    let email = watching(in_flight, &dir, || git::user_email(&dir))?;
    delta.no_email = Some(email.is_none());

    // Two batched calls over the same list.
    let commits = watching(in_flight, &dir, || git::numstat(&dir, &to_score))?;
    let patch_ids = watching(in_flight, &dir, || git::patch_ids(&dir, &to_score))?;

    let scored = score(
        commits,
        &to_score,
        &patch_ids,
        email.as_deref(),
        &state.seen_patch_ids,
    );
    delta.events = scored.events;
    delta.new_patch_ids = scored.new_patch_ids;
    delta.new_awarded = scored.new_awarded;

    Ok(delta)
}

fn resolve(
    path: &Path,
    states: &[RepoState],
    in_flight: &InFlight,
) -> Result<(RepoIdentity, Match)> {
    let identity = watching(in_flight, path, || git::resolve_identity(path))?;
    // Each candidate's store is stat-ed, and any of them can be the one that
    // hangs, so the bracket follows the path being asked about.
    let matched = match_identity(states, &identity, |dir| {
        watching(in_flight, dir, || dir.exists())
    });
    Ok((identity, matched))
}

/// What one pass under the common dir yields.
struct Store {
    /// One entry per live worktree's `logs/HEAD`, whole.
    reflogs: Vec<String>,
    watch: Vec<PathBuf>,
    scan_dir: PathBuf,
}

fn read_store(state: &RepoState) -> Store {
    let mut store = Store {
        reflogs: Vec::new(),
        watch: Vec::new(),
        scan_dir: state.scan_dir().to_path_buf(),
    };

    for worktree in state.live_worktrees() {
        let head_dir = state.head_dir(worktree);
        let logs = head_dir.join("logs");

        // Missing is empty rather than an error: a worktree has no reflog
        // until something writes to its HEAD.
        if let Ok(text) = fs::read_to_string(logs.join("HEAD")) {
            store.reflogs.push(text);
        }

        // It has to be the directory: expire and gc rename `HEAD.lock` over
        // `HEAD`, which leaves a watch on the file holding an orphaned inode.
        store
            .watch
            .push(if logs.is_dir() { logs } else { head_dir });
    }

    store
}

/// Publish the path before touching it and clear it after, so a call that never
/// returns can be told from a slow one.
fn watching<T>(in_flight: &InFlight, path: &Path, touch: impl FnOnce() -> T) -> T {
    in_flight.enter(path);
    let out = touch();
    in_flight.leave();
    out
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Scored {
    events: Vec<Event>,
    new_awarded: Vec<Awarded>,
    new_patch_ids: Vec<String>,
}

/// Walk the commits in `ct` order and price them.
///
/// `order` is the SHAs in first-seen order, which breaks a tie on `ct` — a
/// rebase stamps several commits with the same second. Reflog order alone
/// cannot do the job: it is chronological within one file, but across pooled
/// reflogs it is one worktree's whole history and then another's, which can
/// present a rewrite before the commit it rewrites.
fn score(
    mut commits: Vec<CommitInfo>,
    order: &[String],
    patch_ids: &HashMap<String, String>,
    email: Option<&str>,
    seen_patch_ids: &BTreeSet<String>,
) -> Scored {
    let first_seen: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, sha)| (sha.as_str(), i))
        .collect();

    commits.sort_by_key(|c| {
        (
            c.committed_timestamp,
            first_seen
                .get(c.sha.as_str())
                .copied()
                .unwrap_or(usize::MAX),
        )
    });

    let mut scored = Scored::default();
    // Inserts land as the walk goes, which is what lets a rewrite meet its
    // original inside a single scan.
    let mut paid: HashSet<&str> = seen_patch_ids.iter().map(String::as_str).collect();

    for commit in commits {
        // Merges score nothing, take no patch-id and are never listed. Still
        // recorded, as every arm below is: one left out comes back on every
        // scan until its reflog entry expires.
        if commit.is_merge {
            scored.new_awarded.push(Awarded {
                sha: commit.sha,
                xp: 0,
            });
            continue;
        }

        // No patch-id is nothing to compare against, so it cannot be a
        // rewrite — which is what keeps an `--allow-empty` commit from
        // reading as one.
        let rewrite = match patch_ids.get(&commit.sha) {
            Some(id) => {
                scored.new_patch_ids.push(id.clone());
                !paid.insert(id.as_str())
            }
            None => false,
        };

        // Another author's work is recorded, so a rewrite of it counts as
        // cleanup, but never listed: a shared repo's pane stays your own.
        if !email.is_some_and(|mine| mine.eq_ignore_ascii_case(&commit.author_email)) {
            scored.new_awarded.push(Awarded {
                sha: commit.sha,
                xp: 0,
            });
            continue;
        }

        let xp = if rewrite {
            REWRITE_XP
        } else {
            commit.added + DELETED_WEIGHT * commit.deleted
        };
        scored.new_awarded.push(Awarded {
            sha: commit.sha.clone(),
            xp,
        });
        scored.events.push(Event {
            sha: commit.sha,
            timestamp: commit.committed_timestamp,
            subject: commit.subject,
            added: commit.added,
            deleted: commit.deleted,
            xp,
            rewrite,
        });
    }

    // A SHA `--ignore-missing` dropped — its object gc pruned — is recorded at
    // zero all the same, or the next scan spends both batched calls re-fetching
    // it. The loop above covered what git returned; this is the rest of `order`.
    let returned: HashSet<&str> = scored.new_awarded.iter().map(|a| a.sha.as_str()).collect();
    let dropped: Vec<Awarded> = order
        .iter()
        .filter(|sha| !returned.contains(sha.as_str()))
        .map(|sha| Awarded {
            sha: sha.clone(),
            xp: 0,
        })
        .collect();
    scored.new_awarded.extend(dropped);

    scored
}

/// Bank a delta into the state it came from, and return the XP it earned.
///
/// Writing the state file is the main thread's, as are the markers that live
/// only in memory: the `!`, the stored scan error, the watch table.
pub fn apply(state: &mut RepoState, delta: ScanDelta) -> u64 {
    if let Some(roots) = delta.adopted_roots {
        state.root_commits = roots;
    }
    state
        .awarded
        .extend(delta.new_awarded.into_iter().map(|a| (a.sha, a.xp)));
    state.seen_patch_ids.extend(delta.new_patch_ids);
    state.record(delta.events)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3f2a1b8c9d0e1f2a3b4c5d6e7f8091a2b3c4d5e6";
    const B: &str = "9c4d2e1f0a1b2c3d4e5f60718293a4b5c6d7e8f9";
    const C: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    const MINE: &str = "eve@example.com";

    fn commit(sha: &str, ct: i64, added: u64, deleted: u64) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            author_email: MINE.to_string(),
            committed_timestamp: ct,
            is_merge: false,
            subject: format!("subject of {sha}"),
            added,
            deleted,
        }
    }

    fn ids(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(sha, id)| (sha.to_string(), id.to_string()))
            .collect()
    }

    fn seen(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn order(shas: &[&str]) -> Vec<String> {
        shas.iter().map(|s| s.to_string()).collect()
    }

    /// Score against an empty history, which is a repo's first scan.
    fn score_fresh(commits: Vec<CommitInfo>, patch_ids: &[(&str, &str)]) -> Scored {
        let shas: Vec<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
        let order = order(&shas);
        score(commits, &order, &ids(patch_ids), Some(MINE), &seen(&[]))
    }

    #[test]
    fn xp_is_lines_added_plus_three_times_deleted() {
        let scored = score_fresh(vec![commit(A, 100, 120, 34)], &[(A, "pid-a")]);

        assert_eq!(scored.events[0].xp, 120 + 3 * 34);
        assert!(!scored.events[0].rewrite);
    }

    #[test]
    fn work_already_paid_for_earns_the_flat_two() {
        let scored = score(
            vec![commit(A, 100, 120, 34)],
            &order(&[A]),
            &ids(&[(A, "pid-a")]),
            Some(MINE),
            &seen(&["pid-a"]),
        );

        assert_eq!(scored.events[0].xp, REWRITE_XP);
        assert!(scored.events[0].rewrite);
    }

    #[test]
    fn a_rewrite_meets_its_original_inside_one_scan() {
        // Both unseen, which is every commit on a repo's first scan.
        let scored = score_fresh(
            vec![commit(A, 100, 10, 0), commit(B, 200, 10, 0)],
            &[(A, "pid-a"), (B, "pid-a")],
        );

        assert_eq!(scored.events[0].xp, 10);
        assert_eq!(scored.events[1].xp, REWRITE_XP);
    }

    #[test]
    fn the_original_takes_full_value_whichever_reflog_came_first() {
        // The rewrite is named first, as a pooled second worktree can do, but
        // carries the later committer date.
        let scored = score_fresh(
            vec![commit(B, 200, 10, 0), commit(A, 100, 10, 0)],
            &[(B, "pid-a"), (A, "pid-a")],
        );

        assert_eq!(scored.events[0].sha, A);
        assert_eq!(scored.events[0].xp, 10);
        assert_eq!(scored.events[1].xp, REWRITE_XP);
    }

    #[test]
    fn a_tie_on_committer_date_falls_back_to_reflog_order() {
        // A rebase stamps a run of commits with the same second.
        let scored = score_fresh(
            vec![commit(B, 100, 10, 0), commit(A, 100, 10, 0)],
            &[(B, "pid-a"), (A, "pid-a")],
        );

        assert_eq!(scored.events[0].sha, B);
        assert_eq!(scored.events[1].xp, REWRITE_XP);
    }

    #[test]
    fn a_merge_records_its_sha_and_nothing_else() {
        let mut merge = commit(A, 100, 0, 0);
        merge.is_merge = true;

        let scored = score_fresh(vec![merge], &[]);

        assert_eq!(
            scored.new_awarded,
            vec![Awarded {
                sha: A.to_string(),
                xp: 0
            }]
        );
        assert!(scored.events.is_empty());
        assert!(scored.new_patch_ids.is_empty());
    }

    #[test]
    fn another_authors_commit_is_recorded_but_never_listed() {
        let mut theirs = commit(A, 100, 50, 0);
        theirs.author_email = "someone@else.example".into();

        let scored = score_fresh(vec![theirs], &[(A, "pid-a")]);

        assert!(scored.events.is_empty());
        assert_eq!(
            scored.new_awarded,
            vec![Awarded {
                sha: A.to_string(),
                xp: 0
            }]
        );
        // Recorded, so rewriting their work later is cleanup, not fresh work.
        assert_eq!(scored.new_patch_ids, vec!["pid-a"]);
    }

    #[test]
    fn a_commit_with_no_patch_id_is_not_a_rewrite() {
        // `--allow-empty` produces no patch-id line at all.
        let scored = score_fresh(vec![commit(A, 100, 0, 0)], &[]);

        assert_eq!(scored.events[0].xp, 0);
        assert!(!scored.events[0].rewrite);
        assert!(scored.new_patch_ids.is_empty());
    }

    #[test]
    fn an_email_matches_whatever_its_case() {
        let mut shouted = commit(A, 100, 7, 0);
        shouted.author_email = MINE.to_uppercase();

        assert_eq!(score_fresh(vec![shouted], &[(A, "pid-a")]).events.len(), 1);
    }

    #[test]
    fn no_email_lists_nothing_and_still_records() {
        let commits = vec![commit(A, 100, 120, 34)];
        let scored = score(
            commits,
            &order(&[A]),
            &ids(&[(A, "pid-a")]),
            None,
            &seen(&[]),
        );

        assert!(scored.events.is_empty());
        assert_eq!(
            scored.new_awarded,
            vec![Awarded {
                sha: A.to_string(),
                xp: 0
            }]
        );
        assert_eq!(scored.new_patch_ids, vec!["pid-a"]);
    }

    #[test]
    fn commits_are_scored_oldest_first() {
        let scored = score_fresh(
            vec![
                commit(C, 300, 1, 0),
                commit(A, 100, 1, 0),
                commit(B, 200, 1, 0),
            ],
            &[],
        );

        let order: Vec<&str> = scored.events.iter().map(|e| e.sha.as_str()).collect();
        assert_eq!(order, vec![A, B, C]);
    }

    #[test]
    fn an_event_carries_the_counts_it_was_priced_from() {
        let scored = score_fresh(vec![commit(A, 1756100000, 120, 34)], &[]);

        let event = &scored.events[0];
        assert_eq!((event.added, event.deleted), (120, 34));
        assert_eq!(event.timestamp, 1756100000);
        assert_eq!(event.subject, format!("subject of {A}"));
    }
}
