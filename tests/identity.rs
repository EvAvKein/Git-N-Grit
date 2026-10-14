//! Repo identity: the add flow's reuse/new/picker decision against real git
//! histories, with no `Model` and nothing timing-dependent.

mod common;

use std::fs;

use common::{Added, Fixture, register_into, register_new, scan};
use gng::model::storage::Storage;

fn storage(fixture: &Fixture) -> Storage {
    let storage = Storage::new(fixture.path("storage"));
    fs::create_dir_all(storage.root()).expect("creating the storage root");
    storage
}

#[test]
fn a_moved_repo_reuses_its_state_and_rescans_nothing() {
    let fx = Fixture::new();
    let storage = storage(&fx);
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first");
    let mut state = repo.state();
    storage.save(&state).expect("saving");
    let earned = scan(&mut state);
    storage.save(&state).expect("saving");

    let moved = fx.path("moved");
    fs::rename(&repo.dir, &moved).expect("moving the repo");
    let repo = fx.attach(moved);

    match common::add(&storage, &repo) {
        Added::Registered(mut after) => {
            assert_eq!(after.id, state.id);
            assert_eq!(after.total_xp, earned);
            assert_eq!(after.git_common_dir, repo.identity().git_common_dir);
            assert_eq!(
                scan(&mut after),
                0,
                "the reflog it already scored is not new work"
            );
        }
        Added::Pick { .. } => panic!("expected a reuse, found a picker"),
    }
}

#[test]
fn a_different_project_cloned_over_a_deleted_repos_path_goes_to_the_picker() {
    let fx = Fixture::new();
    let storage = storage(&fx);
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    storage.save(&state).expect("saving");
    scan(&mut state);
    storage.save(&state).expect("saving");
    let original_id = state.id;

    fs::remove_dir_all(&repo.dir).expect("deleting the repo");

    let other = fx.repo("elsewhere");
    other.commit_file("b.txt", "unrelated\n", "first");
    let clone = fx.clone_of(&other, "work");

    match common::add(&storage, &clone) {
        Added::Pick {
            candidates,
            offer_new,
        } => {
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].id, original_id);
            assert!(
                offer_new,
                "an unrelated history should still get to be a new row"
            );

            let fresh = register_new(&storage, &clone);
            assert_ne!(fresh.id, original_id);
            assert_eq!(fresh.total_xp, 0);
            assert_eq!(fresh.root_commits, clone.identity().root_commits);
        }
        Added::Registered(_) => panic!("expected a picker"),
    }
}

#[test]
fn history_reinitialized_in_place_goes_to_the_picker_and_keeps_its_total_when_taken() {
    let fx = Fixture::new();
    let storage = storage(&fx);
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    storage.save(&state).expect("saving");
    let earned = scan(&mut state);
    storage.save(&state).expect("saving");

    fs::remove_dir_all(repo.dir.join(".git")).expect("removing the store");
    repo.run(&["init", "-b", "main"]);
    repo.run(&["config", "user.email", common::MINE]);
    repo.commit_file("b.txt", "fresh\n", "fresh start");

    match common::add(&storage, &repo) {
        Added::Pick {
            candidates,
            offer_new,
        } => {
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].id, state.id);
            assert!(
                offer_new,
                "git cannot tell this from a stranger's repo cloned to the same path"
            );

            let mut taken = register_into(&storage, candidates.into_iter().next().unwrap(), &repo);
            assert_eq!(taken.total_xp, earned);
            assert_eq!(taken.root_commits, repo.identity().root_commits);
            assert!(
                scan(&mut taken) > 0,
                "it earns from the new history going forward"
            );
        }
        Added::Registered(_) => panic!("expected a picker"),
    }
}

#[test]
fn recloning_the_same_upstream_to_the_same_path_reuses_the_state_and_keeps_its_total() {
    let fx = Fixture::new();
    let storage = storage(&fx);
    let upstream = fx.repo("upstream");
    upstream.commit_file("a.txt", "one\n", "first");
    let clone1 = fx.clone_of(&upstream, "work");
    let mut state = clone1.state();
    storage.save(&state).expect("saving");
    let earned = scan(&mut state);
    storage.save(&state).expect("saving");

    fs::remove_dir_all(&clone1.dir).expect("deleting the clone");
    let clone2 = fx.clone_of(&upstream, "work");

    match common::add(&storage, &clone2) {
        Added::Registered(mut after) => {
            assert_eq!(after.id, state.id);
            assert_eq!(after.total_xp, earned);
            assert_eq!(scan(&mut after), 0);
        }
        Added::Pick { .. } => panic!("expected a reuse"),
    }
}

#[test]
fn an_orphan_branch_adds_a_root_but_still_matches_on_intersection() {
    let fx = Fixture::new();
    let storage = storage(&fx);
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\n", "first");
    let mut state = repo.state();
    storage.save(&state).expect("saving");
    let earned = scan(&mut state);
    storage.save(&state).expect("saving");

    repo.run(&["checkout", "--orphan", "gh-pages"]);
    repo.commit_file("index.html", "hi\n", "docs");

    match common::add(&storage, &repo) {
        Added::Registered(after) => {
            assert_eq!(after.id, state.id);
            assert_eq!(after.total_xp, earned);
            assert_eq!(after.root_commits.len(), 2);
        }
        Added::Pick { .. } => panic!("expected a reuse via intersecting roots"),
    }
}

#[test]
fn two_moved_clones_of_one_upstream_reach_the_picker_together() {
    let fx = Fixture::new();
    let storage = storage(&fx);
    let upstream = fx.repo("upstream");
    upstream.commit_file("a.txt", "one\n", "first");

    let clone_a = fx.clone_of(&upstream, "a");
    let clone_b = fx.clone_of(&upstream, "b");
    let state_a = clone_a.state();
    let state_b = clone_b.state();
    storage.save(&state_a).expect("saving");
    storage.save(&state_b).expect("saving");

    let moved_a = fx.path("moved-a");
    let moved_b = fx.path("moved-b");
    fs::rename(&clone_a.dir, &moved_a).expect("moving a");
    fs::rename(&clone_b.dir, &moved_b).expect("moving b");
    let repo_a = fx.attach(moved_a);

    match common::add(&storage, &repo_a) {
        Added::Pick {
            candidates,
            offer_new,
        } => {
            let mut ids: Vec<_> = candidates.iter().map(|s| s.id).collect();
            ids.sort();
            let mut expected = vec![state_a.id, state_b.id];
            expected.sort();
            assert_eq!(ids, expected);
            // Neither store is registered live at `repo_a`'s new path, so a
            // fresh clone landing there would still need its own row.
            assert!(offer_new);
        }
        Added::Registered(_) => panic!("expected the picker for two indistinguishable clones"),
    }
}
