//! The scan against real throwaway repos, since what it does is defined by how
//! `git` actually behaves.

mod common;

use std::fs;

use common::{Fixture, MINE, THEIRS, findings, reached_git, scan, verify};
use gng::model::scan::{apply, run_scan};
use gng::model::storage::Ownership;

#[test]
fn a_commit_scores_lines_added_plus_three_times_deleted() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\nthree\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    // one kept, two dropped, one added
    repo.write("a.txt", "one\nfour\n");
    repo.commit("second");

    assert_eq!(scan(&mut state), 1 + 3 * 2);
}

#[test]
fn another_authors_commit_earns_nothing_and_is_never_listed() {
    let fx = Fixture::new();
    let repo = fx.repo("shared");
    repo.commit_file("a.txt", "mine\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    repo.write("b.txt", "theirs\n");
    let theirs = repo.commit_as(THEIRS, "their work");

    assert_eq!(scan(&mut state), 0);
    assert!(state.events.iter().all(|e| e.subject != "their work"));
    // Recorded all the same, so a rewrite of it reads as cleanup.
    assert_eq!(state.awarded.get(&theirs), Some(&0));
}

#[test]
fn a_repo_with_no_user_email_scores_nothing_and_says_which() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    // An empty local value beats a real ~/.gitconfig's, and `git config`
    // answers with nothing, which is the same path as the key being unset.
    repo.run(&["config", "user.email", ""]);
    let mut state = repo.state();

    let d = findings(&state);
    assert_eq!(d.no_email, Some(true));
    assert!(d.events.is_empty());

    apply(&mut state, d);
    assert_eq!(state.total_xp, 0);
}

#[test]
fn a_rescan_with_nothing_new_leaves_the_no_email_answer_standing() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    repo.run(&["config", "user.email", ""]);
    let mut state = repo.state();
    scan(&mut state);

    // Re-deriving it from a scan that never asked would blink the `!` off a
    // repo that still cannot earn.
    let d = findings(&state);
    assert_eq!(d.no_email, None);
    assert!(!reached_git(&d));
}

#[test]
fn a_scan_that_fails_outright_leaves_the_total_alone() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);
    assert!(earned > 0);

    repo.commit_file("b.txt", "two\n", "second");
    // A HEAD that is neither a symref nor a SHA is not a git repository, while
    // the reflog naming work to score is still perfectly readable.
    fs::write(repo.dir.join(".git/HEAD"), "garbage\n").expect("writing HEAD");

    let err = run_scan(&state).expect_err("a failed scan");
    assert!(
        format!("{err:#}").contains("not a git repository"),
        "{err:#}"
    );
    assert_eq!(state.total_xp, earned);
}

#[test]
fn a_merge_is_never_listed_and_the_scan_after_it_reaches_git_not_at_all() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    repo.run(&["checkout", "-q", "-b", "side"]);
    repo.commit_file("b.txt", "side\n", "side work");
    repo.run(&["checkout", "-q", "main"]);
    repo.commit_file("c.txt", "main\n", "main work");
    repo.run(&["merge", "--no-ff", "-m", "Merge side", "side"]);
    let merge = repo.head();

    let mut state = repo.state();
    scan(&mut state);

    assert!(state.events.iter().all(|e| e.subject != "Merge side"));
    // Its SHA is recorded, or it comes back on every scan until it expires.
    assert_eq!(state.awarded.get(&merge), Some(&0));
    assert!(!reached_git(&findings(&state)));
}

#[test]
fn an_amend_is_a_rewrite_worth_the_flat_two() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    // Not the root commit: amending that one changes `root_commits`, which is
    // a repo-identity question rather than a scoring one.
    repo.commit_file("b.txt", "two\nthree\n", "second");
    let mut state = repo.state();
    let earned = scan(&mut state);

    repo.run(&["commit", "--amend", "-m", "second, reworded"]);

    assert_eq!(scan(&mut state), 2);
    assert!(state.events.last().expect("an event").rewrite);
    assert_eq!(state.total_xp, earned + 2);
}

#[test]
fn a_commit_and_its_rewrite_met_in_one_scan_pay_full_then_two() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    // Both of these are unseen when the scan below meets them, which is the
    // case the pooled-reflog ordering exists for.
    repo.commit_file("b.txt", "two\nthree\n", "second");
    repo.run(&["commit", "--amend", "-m", "second, reworded"]);

    assert_eq!(scan(&mut state), 2 + 2);
    assert!(!state.events[1].rewrite);
    assert!(state.events[2].rewrite);
}

#[test]
fn a_second_scan_awards_only_what_is_new() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    assert_eq!(scan(&mut state), 2);

    repo.commit_file("b.txt", "three\n", "second");

    assert_eq!(scan(&mut state), 1);
    assert_eq!(state.total_xp, 3);
}

#[test]
fn a_scan_with_nothing_in_between_awards_nothing_and_reaches_git_not_at_all() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    let d = findings(&state);
    assert!(d.events.is_empty());
    assert!(!reached_git(&d));
}

#[test]
fn a_reflog_expired_away_leaves_head_unscored() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    repo.run(&["reflog", "expire", "--expire=now", "--all"]);
    let mut state = repo.state();

    // Handed no SHAs, both batched calls would fall back to HEAD and score a
    // commit no reflog named.
    assert_eq!(scan(&mut state), 0);
    assert!(state.awarded.is_empty());
}

#[test]
fn a_rewrite_of_work_that_scored_zero_is_still_worth_two() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    // Binary counts come through as `-` `-`, so this earns nothing — but it
    // has a patch-id, so rewriting it is still cleanup.
    repo.write_bytes("logo.png", &[0, 1, 2, 0, 3]);
    repo.commit("add art");
    assert_eq!(scan(&mut state), 0);

    repo.run(&["commit", "--amend", "-m", "add art, reworded"]);

    assert_eq!(scan(&mut state), 2);
    assert_eq!(state.total_xp, earned + 2);
}

#[test]
fn an_allow_empty_commit_is_not_read_as_a_rewrite() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    repo.run(&["commit", "--allow-empty", "-m", "empty"]);

    // No patch-id at all, so there is nothing for it to collide with.
    assert_eq!(scan(&mut state), 0);
    let last = state.events.last().expect("an event");
    assert_eq!((last.added, last.deleted), (0, 0));
    assert!(!last.rewrite);
}

#[test]
fn the_seed_line_git_worktree_add_writes_parses_and_earns_nothing() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    // Seeds the new reflog with the commit it branched from, on a line
    // carrying no tab and no message.
    let feature = fx.path("feature");
    repo.run(&[
        "worktree",
        "add",
        "-b",
        "feature",
        feature.to_str().expect("a utf-8 temp path"),
    ]);
    let linked = fx.attach(feature.clone());
    state.adopt(&linked.identity(), &feature);

    assert_eq!(scan(&mut state), 0);
    assert_eq!(state.total_xp, earned);
}

#[test]
fn a_branch_tip_merged_back_is_scored_once_across_two_reflogs() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    let path = fx.path("feature");
    repo.run(&[
        "worktree",
        "add",
        "-b",
        "feature",
        path.to_str().expect("a utf-8 temp path"),
    ]);
    let feature = fx.attach(path.clone());
    state.adopt(&feature.identity(), &path);

    feature.commit_file("b.txt", "two\nthree\n", "branch work");
    let earned = scan(&mut state);
    assert_eq!(earned, 2);

    // The tip lands in main's reflog too, with one parent, so nothing skips it
    // as a merge. One `awarded` across the store is what makes it count once.
    repo.run(&["merge", "--ff-only", "feature"]);

    assert_eq!(scan(&mut state), 0);
    // The first commit's 1 and the branch work's 2 — not the 4 a seen-set per
    // worktree would have banked for the branch work alone.
    assert_eq!(state.total_xp, 1 + 2);
}

#[test]
fn work_banked_before_an_expire_is_not_re_minted_after_one() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\nthree\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    repo.run(&["reflog", "expire", "--expire=now", "--all"]);
    repo.run(&["gc", "--prune=now"]);
    repo.commit_file("b.txt", "four\n", "second");

    assert_eq!(scan(&mut state), 1);
    assert_eq!(state.total_xp, earned + 1);
}

#[test]
fn a_reflog_regrown_past_its_old_length_has_every_commit_scored() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    scan(&mut state);
    repo.run(&["reflog", "expire", "--expire=now", "--all"]);

    for n in 0..5 {
        repo.commit_file(&format!("f{n}.txt"), "x\n", &format!("commit {n}"));
    }

    // One line added apiece: nothing skipped by a stale offset into the file.
    assert_eq!(scan(&mut state), 5);
}

#[test]
fn a_reflog_naming_a_pruned_object_scans_cleanly_and_is_not_asked_twice() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    // A reflog entry outlives its object by up to gc.reflogExpire's ninety
    // days, and one bad object aborts the whole batched call without
    // --ignore-missing.
    let ghost = "0123456789abcdef0123456789abcdef01234567";
    let mut log = fs::read_to_string(repo.reflog()).expect("reading the reflog");
    log.push_str(&format!(
        "{} {ghost} Eve <{MINE}> 1787746400 +0000\tcommit: ghost\n",
        repo.head()
    ));
    fs::write(repo.reflog(), log).expect("writing the reflog");

    let d = findings(&state);
    assert!(d.events.is_empty());
    assert!(d.new_awarded.iter().any(|a| a.sha == ghost && a.xp == 0));

    apply(&mut state, d);
    // Recorded, so the next scan spends no batched call skipping it again.
    assert!(!reached_git(&findings(&state)));
}

#[test]
fn a_history_rewritten_in_place_stops_scoring_and_keeps_its_total() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    // Amending the root commit replaces the root this state recorded.
    repo.run(&["commit", "--amend", "-m", "first, reworded"]);
    repo.commit_file("b.txt", "three\n", "second");

    let d = findings(&state);
    assert_eq!(d.ownership, Some(Ownership::Rerooted));
    assert!(d.events.is_empty());

    apply(&mut state, d);
    assert_eq!(state.total_xp, earned);
}

#[test]
fn a_store_replaced_in_place_is_foreign_and_inherits_nothing() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    // A store created after that scan can hold none of what it banked, which
    // is what tells this from the rewrite above — the path is identical.
    fs::remove_dir_all(repo.dir.join(".git")).expect("removing the store");
    repo.run(&["init", "-b", "main"]);
    repo.run(&["config", "user.email", MINE]);
    repo.commit_file("c.txt", "fresh\n", "fresh start");

    let d = findings(&state);
    assert_eq!(d.ownership, Some(Ownership::Foreign));
    assert!(d.events.is_empty());

    apply(&mut state, d);
    assert_eq!(state.total_xp, earned);
}

/// The cheap path returns before it asks whose the store is. A row the app does
/// not hold as ours — its store gone, or last read as someone else's — needs
/// that answer with nothing left to score, or it never leaves that state.
#[test]
fn a_verifying_scan_says_whose_the_store_is_with_nothing_left_to_score() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    let d = verify(&state);

    assert_eq!(d.ownership, Some(Ownership::Ours));
    assert!(d.events.is_empty());
    assert_eq!(d.resolved_roots, Some(state.root_commits.clone()));
    apply(&mut state, d);
    assert_eq!(state.total_xp, earned);
}

#[test]
fn a_verifying_scan_takes_the_roots_of_a_repo_that_had_none() {
    let fx = Fixture::new();
    let repo = fx.repo("empty");
    // Registered before its first commit, so no roots are stored to match on.
    let mut state = repo.state();
    assert!(state.root_commits.is_empty());

    let first = repo.commit_file("a.txt", "one\n", "first");
    // Scored already, so an ordinary scan returns before asking anything.
    state.awarded.insert(first.clone(), 0);
    assert!(!reached_git(&findings(&state)));

    let d = verify(&state);

    assert_eq!(d.ownership, Some(Ownership::Ours));
    assert_eq!(d.resolved_roots, Some(vec![first]));
}

#[test]
fn what_was_awarded_sums_to_the_total() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    // Everything that records without scoring: another author's, and a merge.
    repo.write("b.txt", "theirs\n");
    repo.commit_as(THEIRS, "their work");
    repo.run(&["checkout", "-q", "-b", "side"]);
    repo.commit_file("c.txt", "side\n", "side work");
    repo.run(&["checkout", "-q", "main"]);
    repo.run(&["merge", "--no-ff", "-m", "Merge side", "side"]);
    scan(&mut state);

    repo.commit_file("d.txt", "four\n", "fourth");
    repo.run(&["commit", "--amend", "-m", "fourth, reworded"]);
    scan(&mut state);

    assert!(state.total_xp > 0);
    assert_eq!(state.awarded.values().sum::<u64>(), state.total_xp);
}

#[test]
fn deleting_a_linked_worktrees_directory_leaves_the_repo_scoring_from_the_rest() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);

    let path = fx.path("feature");
    repo.run(&[
        "worktree",
        "add",
        "-b",
        "feature",
        path.to_str().expect("a utf-8 temp path"),
    ]);
    let feature = fx.attach(path.clone());
    state.adopt(&feature.identity(), &path);
    feature.commit_file("b.txt", "two\n", "branch work");
    let branch = scan(&mut state);

    // The checkout is gone; the git dir under the common dir — and so its
    // reflog — is left behind. `git worktree list` calls this *prunable*.
    fs::remove_dir_all(&path).expect("deleting the linked worktree's checkout");

    // Nothing errors over the missing checkout, and the repo goes on scoring
    // from the worktree that is still there.
    assert_eq!(scan(&mut state), 0);
    repo.commit_file("c.txt", "three\n", "main work");
    assert_eq!(scan(&mut state), 1);
    assert_eq!(state.total_xp, earned + branch + 1);
}

#[test]
fn a_separate_git_dirs_working_tree_can_be_deleted_and_still_scores() {
    let fx = Fixture::new();
    let repo = fx.separate_git_dir_repo("work", "work.git");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    let earned = scan(&mut state);
    let parent = repo.head();

    fs::remove_dir_all(&repo.dir).expect("deleting the working tree");
    let store = fx.attach(fx.path("work.git"));

    // No working tree left to `git add` into, so the second commit is built
    // with plumbing straight against the store.
    let blob_path = fx.path("blob.txt");
    fs::write(&blob_path, "two\nthree\n").expect("writing a blob");
    let blob = store.run(&[
        "hash-object",
        "-w",
        blob_path.to_str().expect("a utf-8 temp path"),
    ]);
    store.run(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "100644",
        &blob,
        "b.txt",
    ]);
    let tree = store.run(&["write-tree"]);
    let commit = store.run(&["commit-tree", &tree, "-p", &parent, "-m", "second"]);
    store.run(&["update-ref", "-m", "commit: second", "HEAD", &commit]);

    // `scan_dir` falls back to the store once no live worktree's files are
    // there, rather than failing outright on the one that is gone.
    assert_eq!(scan(&mut state), 2);
    assert_eq!(state.total_xp, earned + 2);
}

#[test]
fn a_binary_file_counts_zero_and_text_beside_it_still_counts() {
    let fx = Fixture::new();
    let repo = fx.repo("work");
    repo.commit_file("seed.txt", "seed\n", "first");
    let mut state = repo.state();
    scan(&mut state);

    repo.write_bytes("logo.png", &[0, 159, 146, 150, 0]);
    repo.write("readme.md", "hello\nthere\n");
    repo.commit("art and words");

    assert_eq!(scan(&mut state), 2);
}
