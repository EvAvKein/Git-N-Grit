# Git-N-Grit — implementation plan

The build order for the proof of concept `PROOF_OF_CONCEPT.md` specifies. That document is the source
of truth for behaviour; this one is the order the code goes in, the shape of each module, and the
facts about `git` and the crates that the implementation depends on.

The stages are built one at a time, each pausing for review before the next one starts. The review is
what the commit waits on, so the working tree holds the finished stage until then.

A stage heading marked ✅ is built and tested.

## Ground truth

Checked against git 2.47.3 and rustc 1.98.0:

- `git log --ignore-missing --no-walk=unsorted --stdin --numstat --format='C%x00…'` emits the format
  line, a blank line, then `added\tdeleted\tpath` rows. `%p` is empty for a root commit.
- `git log --ignore-missing --no-walk --stdin -p | git patch-id --stable` emits `<patch-id> <sha>`,
  keyed off the `commit <sha>` line of the default format. That call must not carry a `--format`, or
  there is no such line for `patch-id` to read.
- Handed empty stdin, both calls fall back to HEAD and score a commit no reflog named.
- `rev-parse --path-format=absolute --git-dir --git-common-dir` returns two absolute lines.
- `git var GIT_COMMITTER_IDENT` ends in the current local offset — `Eve <e@x> 1787766661 +0300`.
- `File::try_lock` is stable, failing with `TryLockError::{WouldBlock, Error}`.
- `ratatui` 0.30.2 re-exports `ratatui::crossterm` and provides `init()` / `restore()`.
- `notify` 8.2's `EventHandler` is blanket-implemented for `FnMut(notify::Result<Event>) + Send`, so
  the forwarder is a closure holding the `Sender<AppEvent>`.
- `park`/`unpark` are permit-based: an `unpark` arriving before the `park` makes that `park` return
  at once, so there is no lost wakeup to guard against.
- A `stat` blocked on a dead mount takes no signal and reaches no cancellation point, so a thread in
  it cannot be ended — only outlived.

## Modules

```
src/
  lib.rs      re-exports the modules below
  main.rs     bin `gng`: lock, second-instance screen, run
  lock.rs     .take exclusive lock, identity line, .takeover handover
  event.rs    AppEvent: the one message type every thread sends the main loop
  git.rs      every subprocess: rev-parse, rev-list, config, var, the two batched scan calls
  reflog.rs   logs/HEAD → the SHAs it names (pure)
  probe.rs    the path a thread is inside a filesystem call on, and the wedge rule
  stale.rs    the 1s probe of stale stores, and the heartbeat that says it is wedged
  scan.rs     the three steps, scoring, the worker thread
  state.rs    RepoState, load/save, identity resolution and matching
  app.rs      App model, modes, key handling, event loop
  ui.rs       rendering: two panes, prompt, picker, confirm, labels, relative times
tests/        integration tests over throwaway git repos
```

## 1 — Cargo manifest ✅

`cargo add ratatui notify@8.2 serde --features derive serde_json anyhow dirs uuid --features v4`,
then the `[lib]` and `[[bin]]` stanzas, both named `gng`.

## 2 — `reflog.rs` ✅

Pure, and unit-tested before anything calls it.

`extract_shas(reflogs: &[String]) -> Vec<String>` — split each line on whitespace, take field 2,
dedupe keeping first-seen order across the pooled reflogs, drop the all-zeros SHA. Each entry is one
`logs/HEAD`'s whole text; reading them is the caller's. The message-less line `git worktree add`
writes parses through the same path as every other line.

## 3 — `git.rs` ✅

Thin wrappers, each returning `anyhow::Result` over captured stdout:

- `resolve_identity(path) -> RepoIdentity { root_commits, git_common_dir, head_subdir }` — `rev-list
  --max-parents=0 --all` sorted, plus the two `rev-parse` lines; `head_subdir` is the git dir with the
  common dir stripped from its front, relative so it joins back onto it.
- `user_email(path) -> Option<String>` — `config user.email`. Exit 1 is the key unset and so a
  `None`; every other exit is an error. A path that has gone away exits 128, which as a `None` would
  read as a repo with no identity configured rather than a scan that failed.
- `local_utc_offset() -> i32` — the trailing field of `git var GIT_COMMITTER_IDENT`.
- `numstat(path, shas)` and `patch_ids(path, shas)` — each spawns a thread to write the SHA list into
  the child's stdin while the caller drains stdout, and each returns early on an empty `shas` rather
  than letting git fall back to HEAD. `--ignore-missing` precedes `--stdin`. `patch_ids` pipes `git
  log … -p` into `git patch-id --stable` by handing the first child's stdout to the second as
  `Stdio::from`.

Parsing is separate from spawning, so the parsers unit-test against fixture text:
`parse_numstat(&str) -> Vec<CommitInfo>`, where a `-` count contributes zero, and
`parse_patch_ids(&str) -> HashMap<String, String>` keyed by SHA.

## 4 — `state.rs` ✅

`RepoState` mirrors the spec's JSON field for field, at `file_format_version: 1`.

`Storage { root: PathBuf }` owns the directory and every path under it — `root/repos/`,
`root/.take`, `root/.takeover`. `main` builds it from `--storage` where given and
`dirs::data_dir()?.join("git-n-grit")` otherwise, then passes it down; no module below reaches for
`dirs` itself. Every test builds one on its own temp dir.

- `Storage::load_all()` — enumerate `repos/*.json`, sort by `added_at` then `git_common_dir`,
  returning `Loaded { repos, errors: Vec<LoadError> }`. A file that fails to parse, is named
  something other than a UUID, or carries a `file_format_version` above this build's costs only its own repo.
  `LoadError` keeps the `path` rather than a formatted message: stage 8's row labels itself from the
  file name and stage 7's `d` deletes that file. A `path` equal to `repos_dir()` is the directory
  itself — the read failing, or an entry under it that would not — and both stages compare against
  it rather than asking the filesystem what kind of path it is.
- `mark_duplicates(&mut [RepoState], &HashMap<Uuid, SystemTime>)` — runs at the end of `load_all`
  and sets each state's `Duplicate`. States sharing a `git_common_dir` are `Scoring` for the newest
  file and `Shadowed` for the rest, or `Undecided` for all of them where an mtime is missing from the
  map, which is a file that read but would not say when it was written. The mtimes are a parameter,
  so the rules test without a disk the way `match_identity_with`'s liveness does.
- `Storage::save(&RepoState)` — serialize to `<id>.json.tmp` beside the target, then `fs::rename`
  over `<id>.json`.
- `match_identity(&[RepoState], &RepoIdentity) -> Match` — `Reuse(uuid)` where exactly one state's
  `root_commits` intersects and its `git_common_dir` is equal or gone; `Pick(Vec<uuid>)` where
  several match, or where none match and live states hold this `git_common_dir` — all of them; `New`
  otherwise. A commitless repo matches on `git_common_dir` alone and adopts `root_commits` on the
  first scan that finds one.
- `Storage::adopt(&mut RepoState, &RepoIdentity, source_dir)` — the reuse path: refresh
  `root_commits` and `git_common_dir`, put this `head_subdir` and the `source_dir` it was added by
  into `worktrees` — appended where the `head_subdir` is new, and overwriting the stored `source_dir`
  where it is already listed, since a repo that has moved matches an entry it must not keep the old
  path of — and save.
  `app` calls it on a `Reuse` and on a picker landing on an existing row; `New` mints a v4 UUID and
  an `added_at` instead.
- `RepoState::is_stale` and `live_worktrees` are `Path::exists` probes, so they are stage 6's to call
  and never the main thread's — their doc comments say so, there being no type that can. A *change*
  in the answer is acted on only where stage 7 says: startup, an accepted add, `r`, and a report from
  the stale ticker. Each such change carries a watch to register or drop and a scan to queue, so a
  probe driving the render would clear a `!` with neither.
- `RepoState::ownership(&RepoIdentity, holds_seen_commits) -> Ownership` — whether the store now at
  this state's path is the one it recorded. `Ours` when a root commit git just resolved is one the
  state file already holds, or when the state holds no roots and the common dir matches. `NotYet`
  when git resolved no roots at all and the state holds some. `Rerooted` when what git resolved
  matches none of them but the reflog still names a SHA in `seen_shas`, which a store created after
  those commits were scored cannot. `Foreign` when it matches none of them and names none of them.
  Only `Ours` un-stales a repo or lets its commits score; the caller passes the flag rather than the
  method reaching for a reflog, so the rules test without a disk the way `match_identity`'s do.
- `RepoState::scan_dir()` — the directory the scan's git calls are run in, which is not where its
  reflogs are read from: the first live worktree whose `source_dir` still exists, else
  `git_common_dir`, which serves `config`, `log --numstat` and `patch-id` with no working tree.

## 5 — `probe.rs` and `stale.rs` ✅

`InFlight` holds the path a thread is about to touch and when it started — `enter`, `leave`,
`wedged(limit)` — and the lock is taken only to write it, never across the call, so asking about a
wedged thread never blocks on one. Both threads that touch the filesystem carry one: the ticker
below, and stage 6's worker.

`Ticker` sends into the same `Sender<AppEvent>` every other thread holds, `AppEvent` living in a leaf
`event.rs` rather than in `app.rs` so this is built and tested before `app.rs` exists. `mpsc` cannot
wait on two receivers, so a producer with a channel of its own would need a thread to forward it.

```rust
pub struct Probe { pub id: Uuid, pub git_common_dir: PathBuf }

impl Ticker {
    pub fn spawn(interval: Duration, found: Sender<AppEvent>) -> Ticker;
    pub fn set_probes(&self, probes: Vec<Probe>);      // replaces the set, unparks if non-empty
    pub fn wedged(&self, limit: Duration) -> Option<PathBuf>;
    pub fn abandon(self);                              // stops without joining
}
```

The loop snapshots the probe set, parks with no timeout while it is empty, and otherwise stats each
path in turn — `enter` before the call and `leave` after — then
sends the ids whose dir now exists and `park_timeout`s the interval. It returns on the stop flag or a
dropped receiver. `interval` is a parameter so the tests run in milliseconds; `app` passes one second.

`spawn` drops the `JoinHandle` on purpose and `abandon` consumes the `Ticker`: nothing may ever join
a thread that could be inside a stat that does not return.

Tests cover a store that appears being reported, one still missing not being, an empty set staying
parked, `set_probes` waking a parked ticker, and `abandon` ending the thread — observed through the
sender it drops. `probe.rs` tests the wedge rule directly, as a pure function of a start `Instant`
and a limit: a real hung stat is not something a test can produce, so that path is checked by hand.

## 6 — `scan.rs` ✅

The only thread that stats a path or spawns `git`. Jobs are `Scan(Box<RepoState>)` and
`Resolve { seq, path, states }`.

The job carries the whole state, cloned, so the worker shares nothing with the main thread and can
call `is_stale`, `live_worktrees`, `ownership` and `scan_dir` on it — the methods stage 4 already
documents as the worker's. At a few thousand SHAs and at most 200 events the clone is microseconds,
off the render path.

`ScanFindings { events, new_shas, new_patch_ids, adopted_roots, no_email: Option<bool>,
ownership: Option<Ownership>, stale, watch }`. No `xp_earned`: `RepoState::record` already returns
what it banked and keeps `total_xp` whole across the events cap, and every commit that earns emits an
event. `stale` and `watch` are the liveness it resolved on the way in: a job goes to every repo, not
every live one, and a stale store returns at step 0. `watch` is the resolved directory per live
worktree — its `logs`, or its git dir where no `logs/` exists yet — since deciding that is a `stat`.

`Resolve` answers `AppEvent::Resolved { seq, result }` for the add prompt, `seq` being the prompt's
generation, so an answer to a prompt that has moved on is dropped. It runs `state::match_identity`
with the liveness closure bracketed, because any one of the candidate stores can be the one that
hangs.

It holds a `probe::InFlight` and is abandoned and replaced as the ticker is, taking its queue with
it, so the app re-posts every job it has no findings for. `Worker::spawn(tx) -> (Worker, Sender<Job>)`
mirrors `Ticker`, the handle carrying `wedged` and `abandon`; dropping the job sender alongside it is
what unblocks a worker waiting on an empty queue.

`no_email` and `ownership` are `Option` because step 2 returns before either question is asked.
`None` means the scan didn't determine it and the previous answer stands — without that, the `!`
would clear on the first scan that found nothing new, which is nearly every scan. `ownership` being
`None` is also how a test tells that a scan reached git not at all.

0. `is_stale`, then `live_worktrees`. Stale ⇒ return `stale: true` and stop.
1. Pool `logs/HEAD` from every live worktree git dir through `reflog::extract_shas`.
2. Subtract `seen_shas`; what is left is `to_score`. Empty ⇒ return empty findings with
   `no_email: None`, without reaching git at all. Otherwise `resolve_identity` and `state.ownership`:
   anything but `Ours` returns findings that score nothing and say which.
3. `user_email` against `state.scan_dir()`.
4. `numstat`, then `patch_ids`, run in that same directory.
5. Sort by `(ct, first_seen_index)` and walk. Two or more parents: record the SHA, with no patch-id
   and no event. Author email not matching case-insensitively: record the SHA and patch-id, emit no
   event. Otherwise `xp = 2` where
   the patch-id is already in the seen set — including one inserted earlier in this same walk — else
   `added + 3 * deleted`. Both sets take their inserts as the walk goes, which is what lets a rewrite
   meet its original within a single scan.
6. Entries of `to_score` that `parse_numstat` never returned are the ones `--ignore-missing`
   dropped. They go into `new_shas` with no patch-id and no event, so the next scan doesn't spend two
   batched git calls on a SHA whose object is gone.

The worker sends `AppEvent::ScanDone(id, Result<ScanFindings>)`. `run_scan(&RepoState)` and
`run_resolve(path, states)` are the same bodies, public so tests drive them with no thread, channel
or timing in the way, and `apply(&mut RepoState, ScanFindings) -> u64` banks findings exactly as stage 7
will — the tests use it rather than a helper that could drift from it.

## 7 — `app.rs`

`AppEvent` is stage 6's, in `event.rs`. What this stage adds:

```rust
enum Mode { List, AddPrompt { input, error, checking: bool }, Picker { identity, candidates, sel }, ConfirmRemove { target, sel } }
enum Row { Add, Repo(Uuid), BadFile(PathBuf) }   // also the remove target
```

The forwarder closure sends raw paths and holds no state of its own; the main thread maps them, since
it owns the watch table. A `HEAD` under a watched `logs` dir marks that repo dirty; a `.takeover`
under the data dir that still exists on inspection runs the quit path, which is what makes the
asker's remove-then-create harmless.

Dirty repos drain into a set and queue once each. A repo with a job in flight is re-queued when its
findings land, and renders its previous total meanwhile. The app keeps the ids it is waiting on, since
an abandoned worker takes its queue with it and they have to be posted again.

Enter in the prompt posts a `Resolve` and sets `checking`; the prompt keeps taking keys and esc
leaves it. Each prompt has a `seq`, bumped on esc and on submit, and a `Resolved` carrying any other
`seq` is discarded — a wedged path never answers at all, and its worker is replaced.

Applying findings adds the XP, extends both seen sets, appends events and truncates to the newest 200
from the front, adopts `root_commits` where they were empty, clears the row's stored scan error, sets
`no_email` where the findings determined it, and writes that one state file. Findings that arrive as
`Err` instead store the error string against that repo and write nothing.

The `!` marker is computed per row from three in-memory conditions — the common dir missing, the last
determined `no_email`, and a stored scan error — none of which live in the state file. `r` queues a
job for every live repo, the same set startup does; an accepted add queues one for that repo alone.

Startup, an add and `r` queue a job for every row, stale ones included, and the findings' `stale` and
`watch` are the liveness answer. Each transition carries its own work. Live → stale: unwatch, and set
the marker. Stale → live: the worker resolved the identity on its way in, and the row un-stales only
where that is `Ownership::Ours` — then `Storage::adopt`, register the watches and score. Any other
answer leaves the row stale, since the path now holds a repo that isn't this one.

`AppEvent::Returned` queues those rows the same way, and the findings' `Ownership` decides: `Ours`
un-stales, `NotYet` leaves the probe to ask again, `Foreign` drops it from the set. The app owns the stale set and
hands it to the ticker with `set_probes` whenever it changes — which is what parks the thread when
the last stale repo returns, and wakes it on a live → stale transition.

The wait is `rx.recv()` where nothing is outstanding and the stale set is empty, and
`rx.recv_timeout(WEDGE_LIMIT)` otherwise — a thread can only wedge while it is inside a call, and
those are the only times one is. It checks `ticker.wedged(WEDGE_LIMIT)` and the worker's on each
event and on every expiry. A path named by either is dropped from the work that named it, its row marked *not
responding*, and that thread replaced — `abandon` the old one, `spawn` a new one, and re-post: the
ticker's remaining probes, or every job with no findings yet. Nothing else moves the `!` or the watch
table.

Keys: `↑↓/jk`, `enter` (acts only on `+ Add repo…`), `a`, `d`, `r`, `q`, and `PgUp`/`PgDn`/`home` for
the right pane. In the prompt every key is text, and only enter and esc leave it. Esc from the list
does nothing. No mouse capture is enabled and no mouse events are read.

`d` on a repo row opens the confirm with Cancel selected; Remove deletes the state file, drops the
repo's watches, and removes the row. On a `BadFile` row the confirm names the file instead of a
total, and Remove deletes it — there are no watches to drop. On `+ Add repo…` it does nothing.

## 8 — `ui.rs`

`Layout::horizontal` at 45/55, the left constrained to 28…40 columns. Left pane: `+ Add repo…`, then
one row per repo with its total, `>` on the selection, scrolled to keep the selection visible, rows
dimmed with `!` on any of the three conditions. Labels are the last component of the main worktree's
`source_dir` — of the first listed entry where only linked worktrees are registered — extended
leftward on collision. `BadFile` rows come after every repo row, ordered by file name, dimmed with
`!` and labelled by that name; the right pane carries the `LoadError` message.

Right pane: the selected repo's label and total, then events newest first, two lines each — the
subject with `(redo)` where `rewrite`, then `+N −M` or `rewritten` against the xp. A row carrying `!`
renders the spec's one-line reason in place of the list. An empty list renders nothing but the
`+ Add repo…` row and a bare right pane.

Relative times run off the startup offset, with civil dates from a days-to-y/m/d conversion in the
module.

## 9 — `lock.rs` and `main.rs`

`main` walks `std::env::args()` for `--storage=<path>` and `--storage <path>`, falling back to
`dirs::data_dir()?.join("git-n-grit")`, and rejects anything else with a usage line and a non-zero
exit. That is the whole CLI.

Open `.take` `read(true).write(true).create(true)` and `try_lock`. On success, truncate and write
`<pid> <tty> <now>`, keeping the handle for the process's lifetime; the tty is
`fs::read_link("/proc/self/fd/0")`, or `-`.

On `WouldBlock`, read and validate the line — three whitespace fields, numeric pid, numeric
timestamp. Zero length or malformed re-runs the whole check once after 20 ms; still unreadable is
reported on stdout with a non-zero exit. A line that validates, with `stdin().is_terminal()`, renders
the two-option screen; without a terminal it prints the same to stdout and exits non-zero.

"Move it here" removes any `.takeover`, writes it with our pid, and retries `try_lock` every 50 ms
for three seconds with the screen showing the wait. Success unlinks it and starts normally; timeout
unlinks it and exits non-zero.

The quit path — `q` and handover alike — blanks `.take`, drops the handle, calls
`ratatui::restore()`, and exits 0. An in-flight scan's findings are dropped unwritten.

## 10 — Watcher and the stale ticker

One `notify::RecommendedWatcher` over the `logs` directory of every live worktree,
`RecursiveMode::NonRecursive`, plus the data directory for `.takeover`. Where a repo has no commits
and no `logs/`, watch the git dir instead. Stale repos are not watched.

Registration has to be re-runnable, since a repo that comes back is watched again mid-session. Verify
both halves when the watcher goes in: that `watch()` on a path already watched updates rather than
duplicates — `inotify_add_watch` is keyed by path, so it should — and what `unwatch()` returns for a
path that is already gone, which is an error to ignore rather than propagate.

The ticker is wired here too: one `Ticker::spawn(Duration::from_secs(1), tx)` beside the watcher, fed
by `set_probes` from the app's stale set. The two are opposites — the watcher covers what exists, the
ticker what doesn't — and a repo crossing between them is handed from one to the other.

## 11 — Tests

Unit tests beside their modules for `reflog::extract_shas`, `parse_numstat`, `parse_patch_ids`,
the XP arithmetic, the label rule and the relative-time formatter.

The stale ticker's own tests pass a millisecond interval; the integration tests that wait on it run
at the app's one second and allow for it.

Integration tests in `tests/` over throwaway repos built with `git init` in a temp dir, covering the
spec's three lists — scan behaviour, state and identity and storage, and running twice. The scan
list and `tests/common/` are stage 6's, since the scan is the part defined by how `git` actually
behaves; the other two lists follow their own stages.

`tests/common/` carries the isolation, since it does not survive being left to a test to remember,
and it takes two forms because a test can set the environment of the git calls *it* makes but not of
the ones `gng` makes. Those inherit the test binary's environment, and `std::env::set_var` is
`unsafe` in edition 2024 and races every other test in the binary.

- Calls the test makes get `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` at `/dev/null` and the four
  identity variables, set per `Command`.
- Calls the library makes are covered by repo-local config written at `git init` — `user.email`,
  `commit.gpgsign`, `log.showSignature`, `core.hooksPath` at an empty dir — since local config beats
  a real `~/.gitconfig`'s.

The one case that only approximates is "no `user.email` set anywhere": local config cannot unset a
global value, so the test sets an empty one, which `git config user.email` answers with nothing and
`git::user_email` reads as `None` by the same path as an unset key.

The helper also hands back a `Storage` rooted in the test's own temp dir. The running-twice tests
spawn the built binary — `env!("CARGO_BIN_EXE_gng")` — with `--storage=<that temp dir>`.

## Verification

- `cargo build` and `cargo clippy` clean.
- `cargo test` — the full list from the spec's Tests section.
- Run `./target/debug/gng`, add this repo, commit from another terminal, and watch the XP arrive with
  no keypress; check that keys stay responsive through a first scan and that the row holds its
  previous total until the findings land.
- Start a second `gng` and take "Move it here": the first exits 0 with `.take` blanked.
- `mv` a registered repo away and back with the app idle: the row dims, then recovers on its own with
  no keypress.
- Point a repo at an sshfs mount and drop the link — the row says the filesystem is not responding,
  and the UI, `q` and handover all still answer. This is the only check for the wedged-stat path.
- Read back `~/.local/share/git-n-grit/repos/*.json` against the shape the spec gives.
