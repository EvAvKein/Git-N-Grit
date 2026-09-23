//! One instance at a time: the lock, the handover, and the CLI in front of them.

mod common;

use std::fs;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{Fixture, scan};
use gng::controller::Controller;
use gng::lock::{self, Acquired, Lock};
use gng::state::Storage;

fn gng(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gng"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running gng")
}

fn storage(fixture: &Fixture) -> Storage {
    let storage = Storage::new(fixture.path("storage"));
    fs::create_dir_all(storage.root()).expect("creating the storage root");
    storage
}

fn held(storage: &Storage) -> Lock {
    match lock::acquire(storage).expect("acquiring") {
        Acquired::Held(lock) => lock,
        Acquired::Busy(holder) => panic!("expected the lock, found {holder:?}"),
    }
}

/// A holder that runs the real controller loop with nothing to draw on, and
/// releases the lock as `main` does on the way out. `Ok` on a clean quit.
fn spawn_holder(storage: Storage) -> thread::JoinHandle<anyhow::Result<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let lock = held(&storage);
        let (tx, rx) = mpsc::channel();
        let mut controller = Controller::new(storage, tx)?;
        ready_tx.send(()).expect("signalling ready");
        let result = controller.run(&rx, |_| Ok(()));
        lock.release();
        result
    });
    ready_rx.recv().expect("holder never came up");
    handle
}

#[test]
fn an_unrecognised_argument_prints_usage_and_creates_nothing() {
    let fixture = Fixture::new();
    let root = fixture.path("storage");
    let out = gng(&[&format!("--storage={}", root.display()), "--bogus"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage: gng"));
    assert!(!root.exists());
}

#[test]
fn a_second_instance_without_a_terminal_reports_the_holder_and_exits() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    let _lock = held(&storage);
    let before = fs::read(storage.lock_path()).expect("reading .take");

    let out = gng(&[&format!("--storage={}", storage.root().display())]);

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("already open"));
    assert_eq!(
        fs::read(storage.lock_path()).expect("reading .take"),
        before
    );
    assert!(!storage.repos_dir().exists());
}

#[test]
fn a_malformed_line_with_the_lock_free_is_overwritten() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    fs::write(storage.lock_path(), "not an identity").expect("seeding");

    let _lock = held(&storage);

    let text = fs::read_to_string(storage.lock_path()).expect("reading .take");
    assert!(text.starts_with(&std::process::id().to_string()));
}

#[test]
fn a_malformed_line_with_the_lock_held_is_reported_and_starts_no_tui() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    let _lock = held(&storage);
    fs::write(storage.lock_path(), "not an identity").expect("clobbering");

    let out = gng(&[&format!("--storage={}", storage.root().display())]);

    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("does not say where"));
}

#[test]
fn a_killed_holder_leaves_a_line_the_next_instance_overwrites() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    fs::write(storage.lock_path(), "1 /dev/pts/9 1\n").expect("seeding");
    // The shell execs into `sleep` and keeps fd 9, so killing it frees the lock.
    let mut holder = Command::new("sh")
        .args(["-c", r#"exec 9<>"$0"; flock -n 9 && exec sleep 60"#])
        .arg(storage.lock_path())
        .spawn()
        .expect("spawning the holder");
    thread::sleep(Duration::from_millis(200));
    assert!(matches!(
        lock::acquire(&storage).expect("acquiring"),
        Acquired::Busy(_)
    ));

    holder.kill().expect("killing the holder");
    holder.wait().expect("reaping it");

    let _lock = held(&storage);
    let text = fs::read_to_string(storage.lock_path()).expect("reading .take");
    assert!(text.starts_with(&std::process::id().to_string()));
}

#[test]
fn a_handover_to_a_holder_that_cannot_answer_times_out_and_leaves_it_held() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    // Held with nothing watching for `.takeover` at all - as unresponsive to it
    // as a holder frozen by `kill -STOP` would be.
    let _lock = held(&storage);

    let taken = lock::take_over(&storage).expect("taking over");

    assert!(taken.is_none(), "the asker took a lock nobody let go of");
    assert!(!storage.takeover_path().exists());
    assert!(matches!(
        lock::acquire(&storage).expect("acquiring"),
        Acquired::Busy(_)
    ));
}

#[test]
fn a_handover_ends_the_holder_cleanly_and_hands_the_lock_over() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    let holder = spawn_holder(Storage::new(storage.root()));

    let taken = lock::take_over(&storage).expect("taking over");

    assert!(taken.is_some(), "the asker did not end up holding the lock");
    holder
        .join()
        .expect("holder panicked")
        .expect("holder did not quit cleanly");
    assert!(!storage.takeover_path().exists());
}

#[test]
fn a_handover_mid_scan_leaves_a_state_file_that_parses() {
    let fixture = Fixture::new();
    let storage = storage(&fixture);
    let repo = fixture.repo("repo");
    for n in 0..20 {
        repo.commit_file("f", &format!("{n}\n"), &format!("commit {n}"));
    }
    let mut state = repo.state();
    scan(&mut state);
    let earned = state.total_xp;
    repo.commit_file("g", "one\ntwo\n", "one more");
    storage.save(&state).expect("saving");

    let holder = spawn_holder(Storage::new(storage.root()));
    let taken = lock::take_over(&storage).expect("taking over");
    assert!(taken.is_some());
    holder
        .join()
        .expect("holder panicked")
        .expect("holder quit");

    let loaded = storage.load_all();
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let total = loaded.repos[0].total_xp;
    assert!(
        total == earned || total == earned + 2,
        "{total} after {earned}"
    );
}
