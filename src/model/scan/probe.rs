//! What a thread is inside a filesystem call on, and how long it has been there.
//!
//! A `stat` on a dead mount does not return, take a signal, or reach a
//! cancellation point, so a thread in one cannot be ended — only outlived. Every
//! thread that touches the filesystem publishes what it is about to touch, so
//! the main thread can tell a slow call from one that will never come back.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The call a thread is inside, and when it started. `None` between calls.
///
/// Held only long enough to write, never across the call itself, so a reader
/// asking about a wedged thread is never blocked by it.
#[derive(Debug, Default)]
pub struct InFlight(Mutex<Option<(PathBuf, Instant)>>);

impl InFlight {
    /// About to touch `path`.
    pub fn enter(&self, path: &Path) {
        *self.lock() = Some((path.to_path_buf(), Instant::now()));
    }

    /// Returned from it.
    pub fn leave(&self) {
        *self.lock() = None;
    }

    /// The path one call has been sitting in for longer than `limit`. Nothing
    /// can free it, so the caller's move is to abandon that thread and start
    /// another whose work leaves this path out.
    pub fn wedged(&self, limit: Duration) -> Option<PathBuf> {
        let in_flight = self.lock();
        let (path, started) = in_flight.as_ref()?;
        wedged_for(*started, limit, Instant::now()).then(|| path.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Option<(PathBuf, Instant)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Split out so the rule tests without a hung call, which is not something a
/// test can produce.
fn wedged_for(started: Instant, limit: Duration, now: Instant) -> bool {
    now.duration_since(started) >= limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_in_flight_is_never_wedged() {
        assert_eq!(InFlight::default().wedged(Duration::ZERO), None);
    }

    #[test]
    fn a_call_that_returned_is_never_wedged() {
        let in_flight = InFlight::default();

        in_flight.enter(Path::new("/mnt/gone"));
        in_flight.leave();

        assert_eq!(in_flight.wedged(Duration::ZERO), None);
    }

    #[test]
    fn a_call_still_running_names_its_path() {
        let in_flight = InFlight::default();

        in_flight.enter(Path::new("/mnt/gone"));

        assert_eq!(
            in_flight.wedged(Duration::ZERO),
            Some(PathBuf::from("/mnt/gone"))
        );
    }

    #[test]
    fn a_call_counts_as_wedged_only_past_the_limit() {
        let now = Instant::now();
        let started = now - Duration::from_secs(10);

        assert!(wedged_for(started, Duration::from_secs(10), now));
        assert!(!wedged_for(started, Duration::from_secs(11), now));
    }
}
