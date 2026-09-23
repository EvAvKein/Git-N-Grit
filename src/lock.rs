//! The instance lock: `.take` held for the process's lifetime, and the
//! `.takeover` handshake that asks a holder to let go.
//!
//! The kernel drops a `flock` with its holder, so there is no stale lock to
//! detect. The identity line inside is for showing to the next instance, never
//! for deciding anything.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::state::Storage;

const RETRY_AFTER: Duration = Duration::from_millis(20);
const HANDOVER_POLL: Duration = Duration::from_millis(50);
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(3);

/// The exclusive lock. Held for as long as this value lives.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Lock {
    /// Blank the identity line, then let go. A killed process leaves its line
    /// behind, but the next holder overwrites it before anything reads it.
    pub fn release(self) {
        let _ = self.file.set_len(0);
    }
}

/// Who holds the lock, as its identity line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// `-` where the holder's stdin did not resolve to a path.
    pub tty: String,
    pub since: i64,
}

impl Holder {
    /// Where to go to find it, and when it started. `utc_offset_secs` puts the
    /// time on the local clock.
    pub fn describe(&self, utc_offset_secs: i32) -> String {
        let secs = (self.since + i64::from(utc_offset_secs)).rem_euclid(86_400);
        let clock = format!("{:02}:{:02}", secs / 3600, secs % 3600 / 60);
        if self.tty == "-" {
            format!("pid {}, since {clock}", self.pid)
        } else {
            format!("{}, since {clock}", self.tty)
        }
    }
}

#[derive(Debug)]
pub enum Acquired {
    Held(Lock),
    Busy(Holder),
}

/// Take the lock, or say who has it. `Err` where it is held and its holder's
/// line still will not read after a retry.
pub fn acquire(storage: &Storage) -> Result<Acquired> {
    let path = storage.lock_path();
    if let Some(lock) = try_lock(&path)? {
        return Ok(Acquired::Held(lock));
    }
    if let Some(holder) = read_holder(&path) {
        return Ok(Acquired::Busy(holder));
    }

    // Acquiring truncates then writes, and releasing blanks then drops the
    // handle; a read that lands between the two sees nothing. A writer that
    // died partway leaves a line that never validates, which the retry reports.
    sleep(RETRY_AFTER);
    if let Some(lock) = try_lock(&path)? {
        return Ok(Acquired::Held(lock));
    }
    match read_holder(&path) {
        Some(holder) => Ok(Acquired::Busy(holder)),
        None => bail!(
            "Git-N-Grit is already running, but {} does not say where",
            path.display()
        ),
    }
}

/// Ask the holder to quit by writing `.takeover`, and take the lock once it
/// has. `None` where it did not let go in time. `.takeover` is gone either way.
pub fn take_over(storage: &Storage) -> Result<Option<Lock>> {
    let takeover = storage.takeover_path();
    // Remove first, so a stale file identical to this one still fires an event.
    let _ = fs::remove_file(&takeover);
    fs::write(&takeover, std::process::id().to_string())
        .with_context(|| format!("writing {}", takeover.display()))?;

    let deadline = Instant::now() + HANDOVER_TIMEOUT;
    let lock = loop {
        match try_lock(&storage.lock_path()) {
            Ok(Some(lock)) => break Ok(Some(lock)),
            Ok(None) if Instant::now() >= deadline => break Ok(None),
            Ok(None) => sleep(HANDOVER_POLL),
            Err(err) => break Err(err),
        }
    };
    let _ = fs::remove_file(&takeover);
    lock
}

/// `None` where somebody else holds it. Opening must not truncate: that would
/// erase the holder's line before we know the lock is not ours to write over.
fn try_lock(path: &Path) -> Result<Option<Lock>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(err)) => {
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
    }

    let line = format!("{} {} {}\n", std::process::id(), tty(), now());
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(Lock { file }))
}

fn read_holder(path: &Path) -> Option<Holder> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    parse_holder(&text)
}

/// Three whitespace-separated fields: a numeric pid, a tty, a numeric time.
fn parse_holder(text: &str) -> Option<Holder> {
    let mut fields = text.split_whitespace();
    let (pid, tty, since) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    Some(Holder {
        pid: pid.parse().ok()?,
        tty: tty.to_string(),
        since: since.parse().ok()?,
    })
}

fn tty() -> String {
    fs::read_link("/proc/self/fd/0")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "-".to_string())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> Storage {
        let root = std::env::temp_dir().join(format!("gng-lock-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("creating a temp dir");
        Storage::new(root)
    }

    fn held(acquired: Acquired) -> Lock {
        match acquired {
            Acquired::Held(lock) => lock,
            Acquired::Busy(holder) => panic!("expected the lock, found {holder:?}"),
        }
    }

    #[test]
    fn a_valid_line_parses() {
        let holder = parse_holder("12345 /dev/pts/3 1756100000\n").expect("parses");
        assert_eq!(
            holder,
            Holder {
                pid: 12345,
                tty: "/dev/pts/3".into(),
                since: 1_756_100_000,
            }
        );
    }

    #[test]
    fn empty_short_long_and_non_numeric_lines_do_not_parse() {
        for line in [
            "",
            "12345 /dev/pts/3",
            "1 2 3 4",
            "abc /dev/pts/3 1",
            "1 t x",
        ] {
            assert_eq!(parse_holder(line), None, "{line:?}");
        }
    }

    #[test]
    fn the_time_is_reckoned_on_the_local_clock() {
        let holder = Holder {
            pid: 1,
            tty: "/dev/pts/3".into(),
            since: 3600 * 6 + 60 * 14,
        };
        assert_eq!(holder.describe(3 * 3600), "/dev/pts/3, since 09:14");
        assert_eq!(holder.describe(-7 * 3600), "/dev/pts/3, since 23:14");
    }

    #[test]
    fn a_holder_with_no_tty_is_named_by_pid() {
        let holder = Holder {
            pid: 77,
            tty: "-".into(),
            since: 0,
        };
        assert_eq!(holder.describe(0), "pid 77, since 00:00");
    }

    #[test]
    fn a_second_acquire_finds_the_first_holder() {
        let storage = storage();
        let _lock = held(acquire(&storage).expect("first acquire"));
        match acquire(&storage).expect("second acquire") {
            Acquired::Busy(holder) => assert_eq!(holder.pid, std::process::id()),
            Acquired::Held(_) => panic!("took a lock that was held"),
        }
    }

    #[test]
    fn release_blanks_the_line_and_frees_the_lock() {
        let storage = storage();
        held(acquire(&storage).expect("acquire")).release();
        assert_eq!(fs::read(storage.lock_path()).expect("reading .take"), b"");
        held(acquire(&storage).expect("acquire again"));
    }

    #[test]
    fn a_line_left_by_a_dead_holder_is_overwritten() {
        let storage = storage();
        fs::write(storage.lock_path(), "99999 /dev/pts/9 1\n").expect("seeding");
        let _lock = held(acquire(&storage).expect("acquire"));
        let text = fs::read_to_string(storage.lock_path()).expect("reading .take");
        assert_eq!(parse_holder(&text).map(|h| h.pid), Some(std::process::id()));
    }

    #[test]
    fn a_held_lock_with_an_unreadable_line_is_an_error() {
        let storage = storage();
        let lock = held(acquire(&storage).expect("acquire"));
        fs::write(storage.lock_path(), "garbage").expect("clobbering");
        assert!(acquire(&storage).is_err());
        drop(lock);
    }

    #[test]
    fn a_takeover_that_nobody_answers_times_out_and_cleans_up() {
        let storage = storage();
        let _lock = held(acquire(&storage).expect("acquire"));
        assert!(take_over(&storage).expect("take_over").is_none());
        assert!(!storage.takeover_path().exists());
    }
}
