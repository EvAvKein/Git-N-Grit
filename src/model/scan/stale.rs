//! The probe that notices a stale store coming back, and the heartbeat that
//! says the probe itself is stuck.
//!
//! A stale repo has no watch left to fire — the directory holding its reflog is
//! the one that went away — so this is the only part of the app that learns
//! anything without an event to carry it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, Thread};
use std::time::Duration;

use uuid::Uuid;

use crate::model::event::AppEvent;
use crate::model::scan::probe::InFlight;

/// One stale store: the dir to stat, and the state file it answers for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub id: Uuid,
    pub git_common_dir: PathBuf,
}

#[derive(Default)]
struct Shared {
    probes: Mutex<Vec<Probe>>,
    in_flight: InFlight,
    stop: AtomicBool,
}

/// A thread stat-ing the stale stores, reporting the ones that came back.
pub struct Ticker {
    shared: Arc<Shared>,
    thread: Thread,
}

impl Ticker {
    /// Spawn parked. The `JoinHandle` is dropped deliberately: a stat that never
    /// returns must not be something the app can wait on.
    pub fn spawn(interval: Duration, found: Sender<AppEvent>) -> Ticker {
        let shared = Arc::new(Shared::default());
        let handle = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || run(&shared, interval, found))
        };
        Ticker {
            shared,
            thread: handle.thread().clone(),
        }
    }

    /// Replace the set being probed. A non-empty set wakes a parked ticker.
    pub fn set_probes(&self, probes: Vec<Probe>) {
        let wake = !probes.is_empty();
        *lock(&self.shared.probes) = probes;
        if wake {
            self.thread.unpark();
        }
    }

    /// The path a single stat has been sitting in for longer than `limit`. The
    /// answer to one is to [`abandon`](Ticker::abandon) this ticker and spawn
    /// one whose probes leave that path out.
    pub fn wedged(&self, limit: Duration) -> Option<PathBuf> {
        self.shared.in_flight.wedged(limit)
    }

    /// Stop, without waiting: a wedged thread sees the flag only if its stat
    /// ever returns, and the process exits with it still parked in the kernel.
    pub fn abandon(self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.thread.unpark();
    }
}

fn run(shared: &Shared, interval: Duration, found: Sender<AppEvent>) {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }

        let probes = lock(&shared.probes).clone();
        if probes.is_empty() {
            thread::park();
            continue;
        }

        let mut back = Vec::new();
        for probe in probes {
            shared.in_flight.enter(&probe.git_common_dir);
            let exists = probe.git_common_dir.exists();
            shared.in_flight.leave();

            if exists {
                back.push(probe.id);
            }
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
        }

        // A dropped receiver is the app going away.
        if !back.is_empty() && found.send(AppEvent::Returned(back)).is_err() {
            return;
        }
        thread::park_timeout(interval);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    /// A path under a throwaway directory, removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gng-stale-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("creating a temp dir");
            TempRoot(path)
        }

        fn probe(&self, name: &str) -> Probe {
            Probe {
                id: Uuid::new_v4(),
                git_common_dir: self.0.join(name),
            }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const QUICK: Duration = Duration::from_millis(10);
    const SLACK: Duration = Duration::from_secs(2);

    /// The ids one report carries. `AppEvent` holds an `anyhow::Error` and so
    /// cannot derive `PartialEq`, which is why these are not `assert_eq`s on
    /// the event itself.
    fn returned(event: AppEvent) -> Vec<Uuid> {
        match event {
            AppEvent::Returned(ids) => ids,
            other => panic!("expected Returned, got {other:?}"),
        }
    }

    #[test]
    fn a_store_that_comes_back_is_reported() {
        let temp = TempRoot::new();
        let (outbox, inbox) = mpsc::channel();
        let ticker = Ticker::spawn(QUICK, outbox);
        let probe = temp.probe("gone.git");

        ticker.set_probes(vec![probe.clone()]);
        fs::create_dir(&probe.git_common_dir).expect("creating the store");

        assert_eq!(
            returned(inbox.recv_timeout(SLACK).expect("a report")),
            vec![probe.id]
        );
    }

    #[test]
    fn a_store_still_missing_is_not_reported() {
        let temp = TempRoot::new();
        let (outbox, inbox) = mpsc::channel();
        let ticker = Ticker::spawn(QUICK, outbox);

        ticker.set_probes(vec![temp.probe("gone.git")]);

        assert!(inbox.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn an_empty_set_parks_rather_than_spinning() {
        let (outbox, inbox) = mpsc::channel::<AppEvent>();
        let _ticker = Ticker::spawn(QUICK, outbox);

        assert!(inbox.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn setting_probes_wakes_a_parked_ticker() {
        let temp = TempRoot::new();
        let (outbox, inbox) = mpsc::channel();
        // An interval long enough that only the unpark can deliver this.
        let ticker = Ticker::spawn(Duration::from_secs(600), outbox);
        let probe = temp.probe("here.git");
        fs::create_dir(&probe.git_common_dir).expect("creating the store");

        ticker.set_probes(vec![probe.clone()]);

        assert_eq!(
            returned(inbox.recv_timeout(SLACK).expect("a report")),
            vec![probe.id]
        );
    }

    #[test]
    fn abandoning_ends_the_thread() {
        let temp = TempRoot::new();
        let (outbox, inbox) = mpsc::channel::<AppEvent>();
        let ticker = Ticker::spawn(Duration::from_secs(600), outbox);
        ticker.set_probes(vec![temp.probe("gone.git")]);

        ticker.abandon();

        // The thread owns the sender, so a disconnect is it having returned.
        assert!(matches!(
            inbox.recv_timeout(SLACK),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    /// The rule itself is [`crate::model::scan::probe`]'s; this is the wiring
    /// to it.
    #[test]
    fn a_ticker_that_has_stat_ed_nothing_is_never_wedged() {
        let (outbox, _inbox) = mpsc::channel::<AppEvent>();
        let ticker = Ticker::spawn(Duration::from_secs(600), outbox);

        assert_eq!(ticker.wedged(Duration::ZERO), None);
    }
}
