# Git-N-Grit — implementation plan

The build order for the proof of concept `PROOF_OF_CONCEPT.md` specifies. That document is the source
of truth for behaviour; this one is the order the code goes in, the shape of each module, and the
facts about `git` and the crates that the implementation depends on.

A stage heading marked ✅ is built, tested and committed.

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

## Modules

```
src/
  lib.rs      re-exports the modules below
  main.rs     bin `gng`: lock, second-instance screen, run
  lock.rs     .take exclusive lock, identity line, .takeover handover
  git.rs      every subprocess: rev-parse, rev-list, config, var, the two batched scan calls
  reflog.rs   logs/HEAD → the SHAs it names (pure)
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

## 3 — `git.rs`

Thin wrappers, each returning `anyhow::Result` over captured stdout:

- `resolve_identity(path) -> Identity { root_commits, git_common_dir, worktree }` — `rev-list
  --max-parents=0 --all` sorted, plus the two `rev-parse` lines; `worktree` is the git dir with the
  common dir stripped from its front.
- `user_email(path) -> Option<String>` — `config user.email`, empty output is `None`.
- `local_utc_offset() -> i32` — the trailing field of `git var GIT_COMMITTER_IDENT`.
- `numstat(path, shas)` and `patch_ids(path, shas)` — each spawns a thread to write the SHA list into
  the child's stdin while the caller drains stdout, and each returns early on an empty `shas` rather
  than letting git fall back to HEAD. `--ignore-missing` precedes `--stdin`. `patch_ids` pipes `git
  log … -p` into `git patch-id --stable` by handing the first child's stdout to the second as
  `Stdio::from`.

Parsing is separate from spawning, so the parsers unit-test against fixture text:
`parse_numstat(&str) -> Vec<CommitInfo>`, where a `-` count contributes zero, and
`parse_patch_ids(&str) -> HashMap<String, String>` keyed by SHA.

## 4 — `state.rs`

`RepoState` mirrors the spec's JSON field for field, at `version: 1`.

`Storage { root: PathBuf }` owns the directory and every path under it — `root/repos/`,
`root/.take`, `root/.takeover`. `main` builds it from `--storage` where given and
`dirs::data_dir()?.join("git-n-grit")` otherwise, then passes it down; no module below reaches for
`dirs` itself. Every test builds one on its own temp dir.

- `Storage::load_all()` — enumerate `repos/*.json`, sort by `added_at` then `git_common_dir`. A file
  that fails to parse yields an inline error for that one repo and the rest still load.
- `Storage::save(&RepoState)` — serialize to `<id>.json.tmp` beside the target, then `fs::rename`
  over `<id>.json`.
- `match_identity(&[RepoState], &Identity) -> Match` — `Reuse(uuid)` where exactly one state's
  `root_commits` intersects and its `git_common_dir` is equal or gone; `Pick(Vec<uuid>)` where
  several match, or where none match but a live state already holds this `git_common_dir`; `New`
  otherwise. A commitless repo matches on `git_common_dir` alone and adopts `root_commits` on the
  first scan that finds one.
- `Storage::adopt(&mut RepoState, &Identity)` — the reuse path: refresh `root_commits` and
  `git_common_dir`, append this `worktree` to `worktrees` where it isn't already listed, and save.
  `app` calls it on a `Reuse` and on a picker landing on an existing row; `New` mints a v4 UUID and
  an `added_at` instead.
- Liveness is the caller's: `Path::exists` on each worktree's git dir and on `git_common_dir`.

## 5 — `scan.rs`

`ScanJob { id, worktrees, seen_shas, seen_patch_ids }` → `ScanDelta { id, events, xp_earned,
new_shas, new_patch_ids, adopted_roots, no_email: Option<bool> }`. The job carries cloned seen-sets,
so the worker shares nothing with the main thread; at a few thousand SHAs the clone is microseconds,
and it happens off the render path.

`no_email` is `Option` because step 2 returns before the question is ever asked. `None` means the
scan didn't determine it and the previous answer stands — without that, the `!` would clear on the
first scan that found nothing new, which is nearly every scan.

1. Pool `logs/HEAD` from every live worktree git dir through `reflog::extract_shas`.
2. Subtract `seen_shas`; what is left is `to_score`. Empty ⇒ return an empty delta with
   `no_email: None`, without reaching git at all.
3. `user_email` against the first live worktree's path.
4. `numstat`, then `patch_ids`.
5. Sort by `(ct, first_seen_index)` and walk. Two or more parents: record the SHA, with no patch-id
   and no event. Author email not matching case-insensitively: record the SHA and patch-id, emit no
   event. Otherwise `xp = 2` where
   the patch-id is already in the seen set — including one inserted earlier in this same walk — else
   `added + 3 * deleted`. Both sets take their inserts as the walk goes, which is what lets a rewrite
   meet its original within a single scan.
6. Entries of `to_score` that `parse_numstat` never returned are the ones `--ignore-missing`
   dropped. They go into `new_shas` with no patch-id and no event, so the next scan doesn't spend two
   batched git calls on a SHA whose object is gone.

`spawn_worker(rx: Receiver<ScanJob>, tx: Sender<AppEvent>)` runs the above and sends
`AppEvent::ScanDone(id, Result<ScanDelta>)`.

## 6 — `app.rs`

```rust
enum AppEvent { Input(CtEvent), Fs(Vec<PathBuf>), ScanDone(Uuid, Result<ScanDelta>) }
enum Mode { List, AddPrompt { input, error }, Picker { identity, candidates, sel }, ConfirmRemove { id, sel } }
```

The forwarder closure sends raw paths and holds no state of its own; the main thread maps them, since
it owns the watch table. A `HEAD` under a watched `logs` dir marks that repo dirty; a `.takeover`
under the data dir that still exists on inspection runs the quit path, which is what makes the
asker's remove-then-create harmless.

Dirty repos drain into a set and queue once each. A repo with a job in flight is re-queued when its
delta lands, and renders its previous total meanwhile.

Applying a delta adds the XP, extends both seen sets, appends events and truncates to the newest 200
from the front, adopts `root_commits` where they were empty, clears the row's stored scan error, sets
`no_email` where the delta determined it, and writes that one state file. A delta that arrives as
`Err` instead stores the error string against that repo and writes nothing.

The `!` marker is computed per row from three in-memory conditions — the common dir missing, the last
determined `no_email`, and a stored scan error — none of which live in the state file. `r` queues a
job for every live repo, the same set startup does; an accepted add queues one for that repo alone.

Keys: `↑↓/jk`, `enter` (acts only on `+ Add repo…`), `a`, `d`, `r`, `q`, and `PgUp`/`PgDn`/`home` for
the right pane. In the prompt every key is text, and only enter and esc leave it. Esc from the list
does nothing. No mouse capture is enabled and no mouse events are read.

`d` on a repo row opens the confirm with Cancel selected; Remove deletes the state file, drops the
repo's watches, and removes the row. On `+ Add repo…` it does nothing.

## 7 — `ui.rs`

`Layout::horizontal` at 45/55, the left constrained to 28…40 columns. Left pane: `+ Add repo…`, then
one row per repo with its total, `>` on the selection, scrolled to keep the selection visible, rows
dimmed with `!` on any of the three conditions. Labels are the last component of the main worktree's
`path` — of the first listed entry where only linked worktrees are registered — extended leftward on
collision.

Right pane: the selected repo's label and total, then events newest first, two lines each — the
subject with `(redo)` where `rewrite`, then `+N −M` or `rewritten` against the xp. A row carrying `!`
renders the spec's one-line reason in place of the list. An empty list renders nothing but the
`+ Add repo…` row and a bare right pane.

Relative times run off the startup offset, with civil dates from a days-to-y/m/d conversion in the
module.

## 8 — `lock.rs` and `main.rs`

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
`ratatui::restore()`, and exits 0. An in-flight scan's delta is dropped unwritten.

## 9 — Watcher

One `notify::RecommendedWatcher` over the `logs` directory of every live worktree,
`RecursiveMode::NonRecursive`, plus the data directory for `.takeover`. Where a repo has no commits
and no `logs/`, watch the git dir instead. Stale repos are not watched.

## 10 — Tests

Unit tests beside their modules for `reflog::extract_shas`, `parse_numstat`, `parse_patch_ids`,
the XP arithmetic, the label rule and the relative-time formatter.

Integration tests in `tests/` over throwaway repos built with `git init` in a temp dir, covering the
spec's three lists — scan behaviour, state and identity and storage, and running twice.

A `tests/common/` helper carries both isolations, since neither survives being left to a test to
remember. It runs every git call with `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` at `/dev/null` and
the four identity variables set explicitly, and it hands back a `Storage` rooted in the test's own
temp dir. The running-twice tests spawn the built binary — `env!("CARGO_BIN_EXE_gng")` — with
`--storage=<that temp dir>`.

## Verification

- `cargo build` and `cargo clippy` clean.
- `cargo test` — the full list from the spec's Tests section.
- Run `./target/debug/gng`, add this repo, commit from another terminal, and watch the XP arrive with
  no keypress; check that keys stay responsive through a first scan and that the row holds its
  previous total until the delta lands.
- Start a second `gng` and take "Move it here": the first exits 0 with `.take` blanked.
- Read back `~/.local/share/git-n-grit/repos/*.json` against the shape the spec gives.
