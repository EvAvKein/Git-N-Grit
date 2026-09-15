//! The one thing the main thread waits on.
//!
//! `std::sync::mpsc` cannot wait on two receivers at once, so every producer —
//! the key reader, `notify`'s watcher, the stale ticker and the scan worker —
//! holds a clone of one `Sender<AppEvent>` and constructs its own variant.
//! Nothing forwards between channels, and a send that fails is the app gone.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::crossterm::event::Event as CtEvent;
use uuid::Uuid;

use crate::scan::{Resolved, ScanFindings};

/// Everything that can reach the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// A keypress, or a resize.
    Input(CtEvent),
    /// Paths under a watched directory changed. Raw, because the watch table
    /// that maps them back to repos is the main thread's.
    Fs(Vec<PathBuf>),
    /// A scan finished. `Err` leaves the total alone and takes the row's `!`.
    /// `incarnation` is the worker's, so findings from one that has been
    /// abandoned are dropped rather than banked beside the job that replaced it.
    ScanDone {
        incarnation: u64,
        id: Uuid,
        result: Result<ScanFindings>,
    },
    /// Stale stores whose common dir has reappeared. Whether one is *ours* is
    /// the scan that follows to answer.
    Returned(Vec<Uuid>),
    /// The add prompt's path resolved. `seq` is the prompt's generation, so an
    /// answer to a prompt that has moved on is dropped.
    Resolved { seq: u64, result: Result<Resolved> },
}
