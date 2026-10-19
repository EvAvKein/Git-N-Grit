//! Which state file, if any, a resolved repo belongs to.

use std::path::Path;

use uuid::Uuid;

use super::repo::RepoState;
use crate::model::git::RepoIdentity;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::model::storage::tests::{ROOT_A, ROOT_B, ROOT_C, identity, state};

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
}
