//! Liveness: the watcher, the ticker and the scan worker actually running,
//! against real `mv`s and real elapsed time — the cases `tests/identity.rs`
//! excludes because it drives no `Model` and nothing in it waits on a clock.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use common::Fixture;
use gng::controller::{Controller, UiState};
use gng::event::AppEvent;
use gng::model::InactiveReason;
use gng::state::Storage;
use ratatui::crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

fn storage(fixture: &Fixture) -> Storage {
    let storage = Storage::new(fixture.path("storage"));
    fs::create_dir_all(storage.root()).expect("creating the storage root");
    storage
}

/// A path under a parent distinct from every other path this fixture hands
/// out, so moving something here is never mistaken for a same-parent rename —
/// the tests that mean "moved to an unknown location, discovered only by `r`
/// or the ticker" need a destination the parent watch cannot auto-follow.
fn elsewhere(fixture: &Fixture) -> PathBuf {
    let dir = fixture.path("elsewhere");
    fs::create_dir_all(&dir).expect("creating a different parent");
    dir.join("work")
}

/// A real `Controller` — its watcher, ticker and scan worker all running as
/// threads — driven from the test's own thread, so assertions between pumps
/// see it directly rather than reading state back off disk.
struct Harness {
    controller: Controller,
    rx: Receiver<AppEvent>,
}

impl Harness {
    fn new(storage: Storage) -> Harness {
        let (tx, rx) = mpsc::channel();
        let controller = Controller::new(storage, tx).expect("starting the controller");
        Harness { controller, rx }
    }

    fn press(&mut self, code: KeyCode) {
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        self.controller
            .handle(AppEvent::Input(CtEvent::Key(key)))
            .expect("handling a key");
    }

    fn type_path(&mut self, path: &Path) {
        for c in path.display().to_string().chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// Pump events into the controller until `done` holds, or panic past
    /// `timeout` — the ticker's own poll is one second, so a caller waiting on
    /// it needs a few seconds of headroom.
    fn wait_until(&mut self, timeout: Duration, mut done: impl FnMut(&Controller) -> bool) {
        let deadline = Instant::now() + timeout;
        while !done(&self.controller) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "condition not met within {timeout:?}");
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(event) => self.controller.handle(event).expect("handling an event"),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("the channel closed"),
            }
        }
    }

    /// Add through the real prompt — real git identity resolution, on the
    /// worker thread — and wait for it to land as a registered row.
    fn add(&mut self, dir: &Path) -> Uuid {
        self.press(KeyCode::Char('a'));
        self.type_path(dir);
        self.press(KeyCode::Enter);
        self.wait_until(Duration::from_secs(5), |c| {
            matches!(c.mode(), UiState::List)
        });
        self.controller
            .selected_repo()
            .expect("the row the add landed on")
            .id
    }

    fn total(&self, id: Uuid) -> u64 {
        self.controller.state(id).expect("the repo").total_xp
    }
}

#[test]
fn a_moved_repo_loads_stale_and_keeps_its_total() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let repo = fx.repo("work");
    repo.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let id = {
        let mut h = Harness::new(Storage::new(store.root()));
        let id = h.add(&repo.dir);
        h.wait_until(Duration::from_secs(5), |c| {
            c.state(id).is_some_and(|s| s.total_xp == 2)
        });
        id
    };

    fs::rename(&repo.dir, fx.path("moved")).expect("moving the repo");

    let mut h = Harness::new(Storage::new(store.root()));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });
    assert_eq!(h.total(id), 2);
}

#[test]
fn once_stale_it_recovers_on_its_own_and_scores_what_happened_while_gone() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let original = fx.repo("work");
    original.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let mut h = Harness::new(store);
    let id = h.add(&original.dir);
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 2)
    });

    let dest = elsewhere(&fx);
    fs::rename(&original.dir, &dest).expect("moving the repo away");
    let moved = fx.attach(dest);
    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });

    // Work done while the row is stale — the watch was dropped along with it.
    moved.commit_file("b.txt", "three\nfour\nfive\n", "while gone"); // xp 3
    fs::rename(&moved.dir, fx.path("work")).expect("moving it back");

    // No further key: the ticker's own probe, polling once a second, is what
    // notices the store returned.
    h.wait_until(Duration::from_secs(5), |c| c.inactive_reason(id).is_none());
    assert_eq!(h.total(id), 5);
}

#[test]
fn returning_on_r_re_registers_the_watch_not_just_the_marker() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let original = fx.repo("work");
    original.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let mut h = Harness::new(store);
    let id = h.add(&original.dir);
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 2)
    });

    let dest = elsewhere(&fx);
    fs::rename(&original.dir, &dest).expect("moving it away");
    let moved = fx.attach(dest);
    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });

    fs::rename(&moved.dir, fx.path("work")).expect("moving it back");
    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| c.inactive_reason(id).is_none());
    assert_eq!(h.total(id), 2, "nothing new happened while it was gone");

    // A commit made now, live, with no further key: if the watch was really
    // re-registered rather than the marker merely cleared, this lands on its
    // own.
    let back = fx.attach(fx.path("work"));
    back.commit_file("c.txt", "six\n", "live"); // xp 1
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 3)
    });
}

#[test]
fn a_different_repo_at_a_stale_path_is_foreign_and_inherits_nothing() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let original = fx.repo("work");
    original.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let mut h = Harness::new(store);
    let id = h.add(&original.dir);
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 2)
    });

    fs::rename(&original.dir, elsewhere(&fx)).expect("moving it away");
    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });

    let unrelated = fx.repo("work"); // a fresh git init at the vacated path
    unrelated.commit_file("x.txt", "unrelated\n", "someone else's history");

    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Foreign)
    });
    assert_eq!(h.total(id), 2, "nothing of the foreign repo's is inherited");
}

#[test]
fn a_clone_in_flight_does_not_unstale_but_the_same_upstream_does_once_written() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let original = fx.repo("work");
    original.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let mut h = Harness::new(store);
    let id = h.add(&original.dir);
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 2)
    });

    let dest = elsewhere(&fx);
    fs::rename(&original.dir, &dest).expect("moving it away");
    let backup = fx.attach(dest);
    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });

    // `git init` with no commits yet: what a clone looks like before it has
    // written a single ref.
    fs::create_dir_all(fx.path("work")).expect("recreating the path");
    let seed = fx.attach(fx.path("work"));
    seed.run(&["init", "-b", "main"]);

    h.press(KeyCode::Char('r'));
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });
    assert_eq!(h.total(id), 2);

    // The seed becomes a real clone of the same upstream, with refs written —
    // no further key: the ticker's probe notices it on its own.
    fs::remove_dir_all(fx.path("work")).expect("clearing the seed");
    fx.clone_of(&backup, "work");

    h.wait_until(Duration::from_secs(5), |c| c.inactive_reason(id).is_none());
    assert_eq!(h.total(id), 2);
}

#[test]
fn a_same_parent_mv_is_followed_without_ever_dimming() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let original = fx.repo("work");
    original.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let mut h = Harness::new(store);
    let id = h.add(&original.dir);
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 2)
    });

    let renamed = fx.path("work-renamed");
    fs::rename(&original.dir, &renamed).expect("renaming it in place");

    // No `r`: the parent watch catches the correlated rename and follows it.
    // The lone "from" half of the same rename typically lands first and can
    // mark the row stale for a moment before the relocate's own rescan
    // settles it again — both conditions are waited for together rather than
    // asserted right after the path updates.
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id)
            .is_some_and(|s| s.worktrees[0].source_dir == renamed)
            && c.inactive_reason(id).is_none()
    });

    // A commit made at the new path, live, with no further key: the reflog
    // watch was re-registered there too, not just the label.
    let moved = fx.attach(renamed);
    moved.commit_file("b.txt", "three\n", "after the mv"); // xp 1
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 3)
    });
}

#[test]
fn a_cross_parent_mv_dims_without_a_keypress() {
    let fx = Fixture::new();
    let store = storage(&fx);
    let original = fx.repo("work");
    original.commit_file("a.txt", "one\ntwo\n", "first"); // xp 2

    let mut h = Harness::new(store);
    let id = h.add(&original.dir);
    h.wait_until(Duration::from_secs(5), |c| {
        c.state(id).is_some_and(|s| s.total_xp == 2)
    });

    fs::create_dir_all(fx.path("elsewhere")).expect("creating a different parent");
    fs::rename(&original.dir, fx.path("elsewhere/work")).expect("moving it out from under its parent");

    // No `r`: the parent watch notices it left, even with no known
    // destination, instead of waiting for a manual refresh to find out.
    h.wait_until(Duration::from_secs(5), |c| {
        c.inactive_reason(id) == Some(InactiveReason::Moved)
    });
    assert_eq!(h.total(id), 2);
}
