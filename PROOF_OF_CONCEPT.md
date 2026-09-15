# Git-N-Grit — Proof of Concept

A Ratatui TUI that watches your git reflog and turns your own commits into XP.

You keep using git exactly as you do now, in whatever terminal or editor you like. Git-N-Grit
just sits there, notices the work, and reflects it back at you. It never touches your repo.

Scope of this document is the **first working version only** — levels, streaks and badges are
[out of scope](#out-of-scope).

## Screen

```
┌ Git-N-Grit ────────────────────┬──────────────────────────────────┐
│ + Add repo…                    │ Git-N-Grit          total 1,240xp│
│ > Git-N-Grit          1,240xp  │                                  │
│   dotfiles              380xp  │  1h ago  fix: off-by-one  (redo) │
│   work/api            4,102xp  │          rewritten          +2xp │
│   old/thing  !          210xp  │  2h ago  feat: parse reflog      │
│                                │          +120 −34         +222xp │
│                                │  5h ago  fix: off-by-one         │
│                                │          +3 −3             +12xp │
│                                │  yest.   refactor: drop legacy   │
│                                │          +12 −240         +732xp │
└────────────────────────────────┴──────────────────────────────────┘
  ↑↓/jk move   enter select   a add   d remove
  r refresh   PgUp/PgDn scroll   home newest   q quit
```

Left: the repos you've registered, each with its total. Right: the selected repo's recent
activity, newest first. A `(redo)` always sits above the commit it rewrites, never below — amend,
rebase and cherry-pick each take a fresh committer date, and that is the timestamp the pane sorts on.
Times are relative: `Nm ago` under an hour, `Nh ago` under a day, `yest.` for the previous calendar
day, `MMM D` beyond that.

The calendar day is the local one, from the offset in the trailing field of `git var
GIT_COMMITTER_IDENT`, read once at startup and `+0000` where it doesn't resolve. `std` converts to
UTC alone, and a day reckoned there flips `yest.` three hours into the local day at `+0300`. The
offset is fixed for the session, so a DST transition moves that boundary by an hour until restart.

A row is labelled by the last component of its path, extended leftward a component at a time only
where two rows would otherwise read the same — `work/api` and `old/api` rather than two rows of
`api`.

The `>` marker is the selection, and moving it is what fills the right pane — no enter needed. Enter
acts on the row it sits on, which only `+ Add repo…` does anything with: it opens the same prompt
as `a`. The left pane scrolls to keep the selection in view; a repo added past the bottom would
otherwise be unreachable to select or remove. The right pane scrolls on `PgUp`/`PgDn` with `home`
returning to newest, since the stored events run well past a screenful.

Every control is a key. The app enables no mouse capture and reads no mouse events, so a click is
the terminal's to handle — selecting text across the panes works as it does in any other full-screen
program. Esc backs out of any prompt, picker or confirm without acting; from the list itself there is
nothing to back out of, and it does nothing.

The panes split 45/55, with the left one floored at 28 columns and capped at 40 — a total is worth
more than the tail of a long label, and a repo row is worth more than an empty gutter.

A dimmed `!` row is one that isn't earning: its git dir no longer resolves, or holds a different repo
than the one recorded, leaving it [stale](#state); its history was rewritten at the root; the
filesystem holding it has stopped answering;
it has no `user.email` to match commits against; its last scan failed; or the repo
[appears twice](#state) and the other one is earning. It keeps its total in every case, and the right pane says which in place of the event list:

```
  this repo has moved or been deleted
  a different repo is at this path now
  this repo's history was rewritten since you added it
  this repo's filesystem is not responding
  no user.email is set, so no commit can be matched
  last scan failed: fatal: not a git repository
  this repo appears twice; the other one is earning
  this repo appears twice; neither is earning until you delete one
```

A stale repo stays marked until its store turns up again, which the app checks for once a second. The
`user.email` and scan marks lift on the next scan that reaches git, a repo listed twice when either
of the two is deleted, and a rewritten one when it is added again.

A state file that doesn't load gets a dimmed row of its own, labelled by its filename. It is not a
repo — no total, never scanned — and the right pane carries the reason it wouldn't load. `d` on it
deletes the file.

## XP rules

- Only commits whose **author email** matches the repo's own `user.email` earn anything, compared
  case-insensitively. It is read once per scan — `git -C <path> config user.email`, in the directory
  [git is run in](#scanning) — and never stored, so changing your identity takes effect on the next scan
  with nothing to migrate. Where it resolves to nothing, every commit scores zero, which on screen is
  indistinguishable from having done no work; a repo in that state gets the dimmed `!` and says so.
  A scan with nothing left to score ends before that call and leaves the marker as it stands —
  re-deriving it from a scan that never asked would blink the `!` off a repo that still can't earn,
  which is most scans.
- `xp = lines_added × 1 + lines_deleted × 3`. Deleting code is worth more than writing it.
- A commit whose patch-id has already been recorded — a rebase, amend or cherry-pick of work
  you were already paid for — earns a flat **2 xp** instead. The work counts once; the cleanup
  still counts a little. Patch-ids are tracked per repo, and every non-merge commit a scan scores
  records one whatever it earned: a commit worth zero was still processed, and rewriting it is
  still cleanup.
- Merge commits score nothing, take no patch-id and are never listed. Their SHAs are recorded all
  the same: one left out of `seen_shas` comes back on every scan until its reflog entry expires, each
  spending both batched calls to skip it again.
- The activity pane lists every non-merge commit whose author email matched, whatever it earned — an
  `--allow-empty` commit renders `+0 −0` and `+0xp`. Another author's commit is scored and recorded
  but never listed, so a shared repo's pane stays your own work.

## How it reads git

Shells out to the `git` binary and parses stdout. No git library.

Reflogs are read from git dirs; every `git -C` is run somewhere else. A worktree's git dir can
outlive its files — delete a linked worktree by hand, or the working tree of a
`--separate-git-dir` repo — and `git -C` against a directory that is gone exits 128, which would
fail the whole scan of a repo that is otherwise fine. So git runs in the first live worktree whose
files are still there, and in `git_common_dir` itself where none are: `config`, `log --numstat` and
`patch-id` all answer from a git dir with no working tree.

**1 — SHAs.** Read `logs/HEAD` under the git dir of every live worktree listed in the repo's
state file, and collect the "new" SHA (field 2) of every line, pooled into one set. That's the whole
input, and it's naturally bounded by the reflogs' own expiry, so there is no window setting to pick:
if git still remembers it, it counts.

Split on whitespace to reach field 2. Most lines carry a tab and a message after the timestamp, but
`git worktree add` writes its seed line with neither:

```
0000000000000000000000000000000000000000 215c9bf… Eve <eve@example.com> 1787746422 +0300
```

Dedupe, keeping first-seen order — checkouts and resets name the same SHA again, and worktrees name
each other's — and drop the all-zeros SHA. The zeros are a guard rather than a routine case:
`logs/HEAD` writes them in the *old* field of a `commit (initial)` line, and field 2 is never
all-zeros in ordinary operation.

Then drop every SHA already in [`seen_shas`](#state). What is left is what steps 2 and 3 run on, so
their cost scales with the number left to score rather than the size of the reflog they came from —
in the steady state, one commit.

Where nothing is left, the scan ends here. Both calls take their input on stdin, and handed none they
fall back to HEAD:

```sh
$ printf '' | git log --ignore-missing --no-walk=unsorted --stdin --format=%H
f5d1d2c21b8683e35dc6f5c232db38a5edfa91de     # HEAD
```

which scores a commit no reflog named. A repo whose reflog has just been expired away reaches this
with no filtering at all.

Where something *is* left, the scan resolves the store's identity first and scores nothing unless it
still shows a root commit the state file holds. A path can be refilled by an unrelated project
between one scan and the next — `rm -rf /repo && git clone <other> /repo`, or `rm -rf .git && git
init` in place — and that store is live, watched, and full of commits with the right author email,
so nothing a `stat` can see distinguishes it. The check costs one `rev-list` on a scan that already
has two batched calls to make, and nothing on the scans that end above.

A root can also change with the store untouched: `rebase --root`, an amend of the first commit,
`filter-repo`, `fetch --unshallow`. A reflog still naming a SHA in `seen_shas` tells that from a
replaced store, which can hold none of what was scored before it existed. Such a repo keeps its
total, takes the `!`, and earns again once it is added a second time. A rewrite that expires the
reflog too, as `filter-repo` does, reads as a replacement.

**2 — stats.** Pipe those SHAs into one batched call:

```sh
git log --ignore-missing --no-walk=unsorted --stdin --numstat \
        --format='C%x00%H%x00%ae%x00%ct%x00%p%x00%s'
```

giving author email, timestamp, subject, parents (empty or one = not a merge) and the
added/deleted counts per commit. A binary file's counts come through as `-` `-` and contribute
nothing; text files in the same commit still count.

`--ignore-missing` must come *before* `--stdin`: the reflog can name objects gc has since pruned,
and one is enough to abort the scan with `fatal: bad object`. After `--stdin` the flag is accepted
and does nothing.

A SHA the flag drops is recorded in `seen_shas`, with no patch-id and no event. Objects go
missing under it where the store is borrowed — `clone --shared`, `--reference`, a hand-written
`objects/info/alternates` — and the lender prunes them, or where something outside git removes them.
The reflog entry outlives them by up to `gc.reflogExpire`'s ninety days, and re-minting the work
would take the original object back, which redoing it does not produce: a fresh commit of the same
change carries a fresh committer date and so a different SHA.

**3 — patch-ids.** One more batched call, emitting `<patch-id> <sha>` per commit:

```sh
git log --ignore-missing --no-walk --stdin -p | git patch-id --stable
```

Key the result by SHA: the output is not in input order, and merges and empty commits produce no
line at all. A non-merge without one has no patch-id to record and cannot be a rewrite.

**Order.** Score in `ct` ascending order, tie-breaking on step 1's first-seen order.
Scoring depends on a rewrite meeting its original already paid, or the flat 2 lands on the wrong one
of the pair. Reflog order alone can't deliver that once the reflogs are pooled:
it is chronological *within* one file, but across the pool it is worktree A's whole history and then
worktree B's, which can present a rewrite before the commit it rewrites. The first scan after a repo
is added is exactly the case, since that is when both are unseen. `ct` comes free with step 2, and
the tie-break covers a rebase that stamps several commits with the same second.

**Threading.** Both batched calls read stdin and write unbounded stdout, so write the SHA list
from a spawned thread — feeding stdin and draining stdout from one thread deadlocks as soon as a
pipe buffer fills.

**When a scan runs.** At startup for every live repo, on a successful add for that repo, on `r` for
every live repo, and on the reflog events [Liveness](#liveness) describes. `r` covers the whole list
rather than the selected row: a scan with nothing new costs no git call at all, so scoping it would
save nothing and leave a missed commit needing a walk down every row to find. Those first three also
re-resolve which repos and worktrees are live.

**Where it runs.** On one worker thread — not on the main thread, and not one thread per scan. The
main thread posts a job per dirty repo; the worker runs the three steps and the scoring and sends
back its findings — new events, xp earned, new SHAs, new patch-ids — as one more `AppEvent`. The main
thread applies it and writes the state file, so state keeps a single owner and the write stays on
the path `q` and handover already take.

Every `stat` and every `git` the app runs happens there as well, and none on the main thread. A path
that has stopped answering keeps the thread that touched it, and the main thread is the one that has
to go on drawing and answering `q`. So liveness is the worker's answer too: the main thread queues a
job for every repo rather than every live one, having no way to know which is which, and the findings
report what the worker found — whether the store is still there, and which worktrees to watch. A
stale one is answered by the probe alone, with no git call behind it. Until the first scans land
nothing is known to be live, and rows render without `!`.

A repo with a job in flight is not queued again. Events arriving meanwhile mark it dirty, and it is
re-queued when its findings land. Its row goes on rendering the total it already had: most scans find
nothing, and those have to look like nothing happened.

**Whole file, every time.** No stored offset, no tail read. The reflog is bounded by its own
expiry, so it stays in the tens of kilobytes and parses in single-digit milliseconds even at
implausible commit rates, and `seen_shas` makes re-reading it idempotent.

Reading it incrementally is a post-PoC step, and it has one requirement: anchor the resume point to
content, not to a byte position. Remembering the last line consumed and resuming after it would
survive an expire; a byte offset alone would not reveal that the file had been rewritten underneath
it.

## State

One **state file** per **object store** under `dirs::data_dir()/git-n-grit/repos/` —
`~/.local/share/git-n-grit/repos/` on Linux. A repo and every linked worktree of it share one state
file, one total and one pair of seen-sets. No global state file and no index:

```
repos/
  7c1f4a09-e2b3-4d65-9f01-2a4b6c8d0e1f.json
  b40e8d12-77a5-4c39-8e6b-1f2093ac5d84.json
```

The filename is a v4 UUID, assigned once and never changed — including when a commitless repo gains
its first commit. It names the record and is read back as its id — a file whose name is not a UUID
does not load — but nothing locates a repo by it: the left pane orders by `added_at`, and a state
file is found by its [identity](#repo-identity).

```json
{
  "file_format_version": 1,
  "root_commits": ["9e21c0f…"],
  "git_common_dir": "/home/eve/repo/Git-N-Grit/.git",
  "added_at": 1756100000,
  "total_xp": 1240,
  "seen_shas": ["a0f0365…"],
  "seen_patch_ids": ["84b8408…"],
  "worktrees": [
    { "head_subdir": "",                  "source_dir": "/home/eve/repo/Git-N-Grit" },
    { "head_subdir": "worktrees/feature", "source_dir": "/home/eve/repo/feature"   }
  ],
  "events": [
    { "sha": "a0f0365…", "timestamp": 1756100000, "subject": "feat: parse reflog",
      "added": 120, "deleted": 34, "xp": 222, "rewrite": false }
  ]
}
```

Worktrees share a state file because they share the work, and two reflogs name the same commits. `git
worktree add` seeds the new reflog with the commit it branched from — already scored in the main
worktree — and a fast-forward merge back into main puts the branch tip into main's reflog, where it
has one parent and so is not skipped as a merge. One `seen_shas` and one `seen_patch_ids` across the
whole store is what makes that work count once. A set per worktree would pay for it twice at full
value.

A worktree entry names two directories. `head_subdir` locates the dir holding this worktree's own
`HEAD`, and so its `logs/HEAD`. It is stored relative to `git_common_dir` — empty for the main
worktree and `worktrees/<name>` for a linked one, which is git's own layout. `source_dir` is where the
files are: what the left pane labels the row from, and where [git is run](#scanning).

Both are stored because neither yields the other. `source_dir + "/.git"` is wrong for a linked
worktree or a submodule, where `.git` is a *file* pointing elsewhere; for `git init
--separate-git-dir`; for a bare repo, which has no `/.git` to append; and whenever the add prompt is
handed a subdirectory and `rev-parse` resolves upward.

A scan rewrites one state file, so the cost of a write tracks the store that changed rather than
everything ever registered — and a scan fires on every commit. A failed write costs one store's XP
and no other's, which matters because a lost `seen_patch_ids` makes every commit still in its reflogs
re-mintable at full value.

`file_format_version` is per state file, so repos migrate independently. A file numbered above this build's own
does not load: serde drops the fields it doesn't know, and the next save would write that truncation
back over the newer writer's work.

The storage root is a parameter rather than a lookup: `main` resolves [`--storage`](#binary) where it
is given and `dirs::data_dir()/git-n-grit` otherwise, then hands the result down, and nothing below
reaches for it again. Tests need a root apiece — one for a repo's state, another for the whole
instance lock — and construct it directly in-process, or pass `--storage` to a spawned `gng`.
Carrying it in the environment instead would serve neither: `std::env::set_var` is `unsafe` in
edition 2024 and races against every other test in the binary.

**Writing.** Serialize to `<id>.json.tmp` in the same directory, then `fs::rename` it over the
target. Writing over `<id>.json` in place would open a window where the file is truncated and not yet
rewritten — `File::create` truncates to zero length *before* the first byte goes out — and a crash
there (Ctrl-C, a panic, a closed terminal) would leave unparseable JSON where a stale but valid state
file had been, which is a lost `seen_patch_ids`. `rename(2)` is atomic within a filesystem, and
writing the temp file to the same directory is what guarantees it is the same filesystem. No `fsync`
and no lock around the write itself: the [instance lock](#one-instance-at-a-time) already excludes a
second `gng`. This covers process death, not power loss; handing over to another instance exits
through the ordinary quit path and so does not widen the window.

**Loading.** Enumerate `repos/*.json` and sort by `added_at`, tie-breaking on `git_common_dir`, for
the left pane's order. A row is labelled from the `source_dir` of its main worktree, or of the first
entry listed where only linked worktrees are registered. Matching `*.json` rather than `*` is what makes a
`.tmp` left by an interrupted write harmless. A state file that fails to load — unparseable,
misnamed, or numbered ahead of this build — skips that one repo instead of aborting startup, and
takes a row of its own carrying the reason. Per-repo isolation buys nothing if one bad state file
still takes down the list, and a repo that vanished from the left pane with nothing on screen to say
why would read as XP simply lost. The `repos` directory failing to read is one row of its own naming
that directory, since what is under it is then unknown rather than damaged.

**A store named twice.** Two state files holding one `git_common_dir` would both watch its reflog and
both score it, so the most recently written file earns and the other keeps its total unwatched. Where
a file's mtime won't read, neither earns — the newest can't be named, and banking into a total picked
by guesswork is worse than banking into none. Deleting either state file settles it. Merging them is
not offered: their totals were earned under histories that can no longer be told apart.

**Stale repos.** Liveness is per worktree and keyed on the git dir, which is what holds the reflog. A
worktree whose git dir is gone is dropped from the watch and scan set; a store whose
`git_common_dir` is gone loads *stale* — it keeps its total, renders dimmed with a `!` in the left
pane, and is neither watched nor scanned. Moving a repo on disk puts it in this state, as does
deleting one. Without the marker a stale repo is indistinguishable from an idle one, and stops
earning XP with nothing on screen to say why.

Liveness is resolved again at startup, on an add and on `r`, both ways: a store whose common dir is
back is watched and scanned once more, and one that has just gone loses its watches and takes the
marker. The [worker](#scanning) answers it, so each of those is a job queued and findings applied
rather than anything the main thread looks up for itself.

A stale store is also probed once a second by a thread of its own, which stats each one's stored
`git_common_dir` and reports the ids that resolve again. It parks while nothing is stale and is woken
when something goes, so an app whose repos are all live wakes for it zero times. A stale repo has no
watch left to fire — the directory holding its reflog is the one that went away — so this is the one
place the app learns something with no event to carry it: a mount returning and a directory moved
back into place both happen in silence.

A returning repo resolves its identity before it earns again, and stays stale unless it still matches
the state file it left. A path deleted and refilled — a different project cloned there — would
otherwise score a reflog that has nothing to do with the total it lands in. That costs one `rev-list`
and one `rev-parse` per repo that came back, and nothing for one that didn't.

Roots once stored have to show themselves again for that to count as a match: a resolve of no roots
leaves the repo stale rather than falling back to the common dir. `git clone` creates `.git` seconds
before it writes a ref, so a probe every second lands inside that window, and the commitless fallback
would hand the clone this row and its total. A state file holding no roots has never scored anything —
the first scan to find one adopts it — so those keep the fallback and risk nothing by it.

The resolve answers one of four ways, and they part company on what happens next. Roots that
intersect are this repo, which un-stales and is watched again. No roots at all is a clone still
writing, so the probe stays and asks a second later. Roots that missed every stored one on a store
still naming scored work are this repo rewritten: it un-stales into the `!` that asks to be added
again, and the probe is dropped. Roots that missed every stored one
are a different project, and that probe is dropped too: the path will go on existing, and re-resolving it
every second forever costs a `rev-list` and a `rev-parse` each time to reach the same answer. The
cost of dropping it is that moving the original repo back over that path changes nothing a `stat` can
see, so it waits for `r` or a restart.

**A probe that doesn't come back.** `Path::exists` is a `stat`, and on a hard-mounted NFS share whose
server is gone, a FUSE mount whose daemon died, an autofs trigger or failing hardware, it blocks
uninterruptibly. Those are the same filesystems a stale repo tends to live on, so the probe aims
where hangs are likeliest.

Nothing can free such a thread. Rust has no thread cancellation; `pthread_cancel` waits for a
cancellation point that a blocked `stat` never reaches; a task in that state takes no signal, which is
why `kill -9` cannot shift a process stuck on a dead NFS mount. So the thread publishes the path it
is about to stat and the time it started, and a call outstanding past ten seconds is taken as
wedged: the app abandons that thread — it exits on its own if the mount ever answers — and spawns a
replacement whose work leaves that path out, so every other repo goes on being watched. The
abandoned threads are bounded by the number of paths that can wedge. The row then reads `this repo's
filesystem is not responding`, which is a different thing from having moved or been deleted.

Both threads that touch the filesystem carry this, the ticker and the [worker](#scanning), and are
abandoned the same way. A worker takes its queue down with it, so the main thread holds the jobs it
has no findings for and posts them again to the replacement.

Deleting a linked worktree's directory by hand leaves its git dir and reflog behind under the common
dir — `git worktree list` calls this *prunable*. That entry stays live and harmless: the working tree
is gone, so nothing further is written to that reflog. It disappears on `git worktree prune`.

The two sets answer different questions: patch-ids, "was this *work* already paid for"; SHAs, "was
this *commit* already processed". Both are needed — without `seen_shas`, every rescan would replay
commits whose own patch-ids are already recorded, and each would look like a rewrite of itself and
bank another 2 xp.

`events` is capped at the newest 200, dropping from the front — commits are walked in `ct` order
and appended, so storage order is chronological ascending and the right pane renders it reversed. It
is display-only: `total_xp` is its own running sum and is never recomputed from the list, so old
events cost nothing to discard.

**`events` is the only prunable field.** `seen_shas` and `seen_patch_ids` are kept whole, forever —
including entries whose reflogs have long since aged them out. Capping them the way `events` is
capped would bring back the double-pay bugs they exist to prevent: a commit still in the reflog whose
patch-id had been forgotten would re-mint at full value, and a `git checkout` of an aged-out commit,
which puts that SHA back into the reflog as a fresh entry, would pay for it a second time.

## Repo identity

A state file is identified by its root commits, not by where it sits. Paths move; root commits do
not.

Registration resolves everything it needs from git:

```sh
git -C <path> rev-list --max-parents=0 --all
git -C <path> rev-parse --path-format=absolute --git-dir --git-common-dir
```

`root_commits` is the first command's output, sorted. `git_common_dir` names the object store, and
`head_subdir` is the git dir with the common dir stripped from its front, leaving a relative path —
empty for a main worktree, `worktrees/<name>` for a linked one. A git dir that does not sit under the
common dir fails the resolve. `--separate-git-dir` repos, bare repos and submodules all
resolve to an empty `head_subdir` and stay distinct by their root commits.

An existing state file matches when its `root_commits` **intersects** the set just resolved, and
either its `git_common_dir` is equal or the stored one no longer exists on disk. On a match, reuse
that state file: refresh `root_commits` and `git_common_dir`, and append this `head_subdir` to
`worktrees` if it is not already listed.

Intersection rather than equality, because the set grows. `git checkout --orphan`, the ordinary way
to start a `gh-pages` or docs branch, adds a second root commit that every worktree then reports:

```
before   8201f79
after    d9324e9 8201f79
```

Under equality that repo would read as brand new and re-score its whole reflog at full value. Unrelated
projects do not share a root commit, and clones of one upstream are held apart by the common dir, so
intersection gives up nothing. Rewriting history at the root does empty it, and such a repo reaches
the picker on its live common dir.

Where nothing matches but live state files already hold the resolved `git_common_dir`, the add
goes to the picker — listing every one of them — instead of opening a new state file. One git dir under two state files would
double-count: both would watch that reflog, and both would score it. Three situations arrive here — a
repo re-initialised in place, a different project cloned over a deleted one's path, and one whose
history was rewritten at the root — and git cannot tell the first two apart, since in both the old
objects are gone and only the path is shared. Only the cloned project wants a new row.

| Situation | Result |
| --- | --- |
| re-clone the same upstream to the same path | reuse — XP preserved |
| `mv` the repo, then re-add it | reuse, common dir updated — the reflog isn't re-scored |
| a different project cloned to a reused path | picker — "new repo" is the pick that inherits nothing |
| `rm -rf .git && git init` in place | picker — the existing row is the pick that keeps its total |
| a fork registered alongside its parent | separate state files — the parent's common dir still exists |
| a linked worktree of a registered repo | joins that repo's state file as another `worktrees` entry |
| an orphan branch adds a second root commit | still matches, on the intersection |
| `rebase --root`, or an amend of the first commit | picker — the existing row keeps its total under the new roots |

The `mv` and reused-path rows fail silently under any path-keyed scheme. A moved repo carries its
`.git`, and therefore its whole reflog, along with it, and would bank every commit a second time at
full value; a reused path holds a repo with no relation to the last one that lived there, and would
inherit its total. A wrong total looks exactly like a right one.

Two clones of one upstream, both moved and both re-added, stay indistinguishable: intersecting root
commits, both stored common dirs gone. When more than one state file matches, the add prompt lists
the candidates by their stored paths and the user picks, with a "new repo" option last. Choosing
arbitrarily merges two histories into one state file; opening a new one re-scores a whole reflog at
full value.

A resolve that yields no root commits matches on the common dir alone and keeps whatever roots were
stored — a repo with no commits yet, which adopts `root_commits` on the first scan that finds one, or
one whose refs are momentarily gone. `git_common_dir` and `head_subdir` resolve either way. A set
that has *changed* still has to intersect; only an empty one falls back.

A shallow clone reports its graft boundary rather than its true root, and `fetch --unshallow`
replaces it. Such a repo re-added after it deepens intersects nothing, and reaches the picker on its
live `git_common_dir`.

## Adding and removing

`a` opens an inline path prompt. The path is accepted only if the `rev-parse` in
[Repo identity](#repo-identity) succeeds, so worktrees and `.git`-as-a-file both work; a worktree
reports its own `…/.git/worktrees/<name>`, which has a `logs/HEAD` of its own. `--path-format=absolute`
is what makes that usable — plain `--git-dir` answers a bare relative `.git`. A rejected path shows
an inline error and leaves the prompt open.

While the prompt is open its keys are text — `a`, `d`, `r` and `q` type themselves rather than
command — so enter and esc are the only ways out of it, submitting and abandoning.

Resolving a typed path means running `git` in it and stat-ing what is already registered, so enter
hands the path to the [scan worker](#scanning) rather than doing either on the main thread. The
prompt says it is checking and goes on taking keys; esc leaves, and an answer arriving for a prompt
that has moved on is dropped. A path on a filesystem that has stopped answering therefore takes the
worker, not the app, and the row it would have created simply never appears.

An accepted path resolves its identity and looks for a state file to reuse. Adding a linked worktree
of an already-registered repo joins that repo's state file rather than opening a new one, and the
left pane keeps showing one row; re-adding something already listed matches itself and changes
nothing. Where several state files match, the prompt becomes a picker: one line per candidate showing
its stored paths and total, then a final "new repo", moved through with ↑↓ and taken with enter; esc
abandons the add, registering nothing. The same picker appears where nothing matches but the
resolved git dir is already registered.

`d` removes the selected row, a stale one included. On `+ Add repo…` it does nothing, that row being
no repo. It confirms first, naming what goes with it:

```
┌ Git-N-Grit ─────────────────────────────────────┐
│   Remove old/thing?                             │
│   Its 210xp is deleted with it.                 │
│                                                 │
│ > Cancel                                        │
│   Remove                                        │
└─────────────────────────────────────────────────┘
```

Cancel is where the selection starts, and esc is the same answer. Taking Remove deletes that state
file and drops the repo's watches. The XP goes too — nothing else holds it. Removal is also the way
back from a wrong pick at the picker, which otherwise merges two histories with no undo.

On an unreadable state file's row the confirm names the filename, there being no total to name, and
Remove deletes that file. Without it, clearing that row would mean leaving the app and hunting down a
UUID by hand. The row naming the `repos` directory has no Remove: every repo sits behind it, and
deleting it is not the answer to it not reading.

## Liveness

One `notify` watcher across the `logs` **directory** of every live worktree, non-recursive, keeping
only events whose filename is `HEAD` — that drops the `HEAD.lock` churn. Several watches can map to
one state file, since a repo's worktrees share it; an event on any marks that state file dirty. Where
a repo has no commits yet and no `logs/` to watch, fall back to the git dir; `watch()` on a missing
path just fails. Stale repos are not watched: their common dir is gone, so there is nothing to
register. The same watcher covers one directory that isn't a repo's — the data directory, filtered
to `.takeover`, described under [One instance at a time](#one-instance-at-a-time).

It has to be the directory. Git appends to the reflog in place most of the time, but expire and gc
write `HEAD.lock` and rename it over `HEAD`, which leaves a watch on the file itself holding an
orphaned inode — silently, so every commit from then on goes unnoticed.

A write triggers a rescan of that repo and a redraw — commit in another pane and the XP appears on
its own. One commit fires around three events (open, modify, close), so drain the channel into a
set of dirty repos and queue each once.

Input, file events, finished scans and returning stores funnel into a single `std::sync::mpsc`
channel, so the main thread waits in one place:

```
thread:  loop { event::read() }   ──┐
notify's own watcher thread       ──┤
stale ticker                      ──┼──> mpsc::Sender<AppEvent> ──> main: rx.recv[_timeout]()
scan worker                       ──┘                                     │
        ▲                                                                 │
        └──────────────────── job per dirty repo ─────────────────────────┘
```

The timeout is the wedge check, and it is armed only when a wedge is possible: a job outstanding, or
a repo stale and so being stat-ed every second. Those are the only times a thread is inside a
filesystem call, and a wedged one sends nothing, so without it the app would learn of a dead mount at
the next keypress and not before. With every repo live and nothing queued, the main thread blocks and
wakes for nothing at all.

`notify` already sends into a channel of your choosing — its `EventHandler` is blanket-implemented
for `FnMut(notify::Result<Event>) + Send`, so a closure holding the `Sender<AppEvent>` is the whole
forwarder, and it needs no thread of ours. That leaves three spawned by hand — the key reader, the
scan worker and the stale ticker — plus the short-lived one each batched call needs to feed its
stdin.

## One instance at a time

`gng` holds an exclusive lock for as long as it runs. It and the handover file live beside `repos/`,
both hidden:

```
~/.local/share/git-n-grit/
  .take                   ← the lock, held for the process's lifetime
  .takeover               ← exists only during a handover
  repos/
    7c1f4a09-e2b3-4d65-9f01-2a4b6c8d0e1f.json
```

Without it, two instances would each load state at startup and rewrite whole state files on every
scan, and the second to write would win outright. The copies couldn't be reconciled afterwards
either: the seen-sets would union cleanly, but `total_xp` is a running sum
[never recomputed](#state) from `events`, so of two divergent totals one is simply wrong. Exclusion
is whole-app rather than per repo, because every instance shares `repos/`.

**Taking it.** `File::try_lock` is `flock(LOCK_EX|LOCK_NB)` and needs no crate. The kernel releases
it when the holder dies, SIGKILL included, so there is no stale lock to detect: no liveness probe, no
start time to compare, no recycled pid mistaken for a live one. Open with
`OpenOptions::new().read(true).write(true).create(true)` — `File::create` truncates on open, erasing
the holder's identity line before we know we don't own the lock. On acquiring, truncate and write
pid, tty, and the time taken:

```
12345 /dev/pts/3 1756100000
```

The tty is `fs::read_link("/proc/self/fd/0")`, so no `ps` and no `libc`; `-` where it doesn't
resolve. The pid is what you'd kill by hand, the tty is where to go instead.

**Releasing it.** A clean exit blanks the file before dropping the handle, so the line does not
outlive the process that wrote it. A process killed outright does leave its line behind — but the
kernel released the lock along with it, so the next instance acquires and overwrites that line rather
than ever reading it.

**Reading it.** Only on `WouldBlock`, and validated rather than trusted: three whitespace-separated
fields, numeric pid, numeric timestamp. A line found while acquiring is simply overwritten.

Where the line is zero length or malformed, re-run the whole check once after ~20 ms. Zero length is
usually the transition — acquiring truncates then writes, releasing blanks then drops the handle —
and a malformed line usually means a writer that died partway. The retry resolves both: it takes the
lock and starts normally, or it finds a line that validates and prompts. A lock still held and still
unreadable is reported on stdout, and that instance exits non-zero.

**A second instance.** Where the line validates, the choice is a rendered screen with the picker
shape of [Adding and removing](#adding-and-removing):

```
┌ Git-N-Grit ──────────────────────────────────────────────────────┐
│                                                                  │
│   Git-N-Grit is already open in another terminal                 │
│                                                                  │
│     /dev/pts/3, since 09:14                                      │
│                                                                  │
│   > Close this one, leave the other running                      │
│     Move it here — the other one will close                      │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
  ↑↓/jk move   enter select   q quit
```

Closing here is a choice rather than a failure, so it exits 0. Where stdin is not a terminal there is
no screen to render and nobody to answer, so it prints the same information to stdout and exits
non-zero, leaving the holder alone. Nothing ends another instance without a choice made at this
screen.

**Handover.** Through `notify` rather than a signal: the holder is already blocked on
[one channel](#liveness), already has a watcher feeding it, and already has a quit path that restores
the terminal and drops the lock.

The asker removes any existing `.takeover`, then writes it with its own pid — remove-then-create, so
an identical stale file still fires an event — and retries `try_lock` every 50 ms for three seconds,
the screen showing the wait. Success unlinks `.takeover` and starts normally; timeout unlinks it,
says the other one didn't answer and is still running in its terminal, and exits non-zero.

The holder watches the data directory non-recursively for the filename `.takeover`, the same shape as
the reflog watch. The event arrives as one more `AppEvent` and runs the path already written for `q`:
finish the state write in flight, blank and release the lock, restore the terminal, exit 0. It has to
work from any UI state, the add-repo prompt included. Handing over mid-scan loses nothing: quitting
does not wait for the worker, its findings are dropped unwritten, and `seen_shas` makes the next
instance's scan redo it to the same result.

**It stops at asking.** A holder too wedged to drain its channel keeps the lock, and the asker exits.
No SIGKILL escalation and no SIGTERM: killing a ratatui process leaves that pane in raw mode with the
alternate screen up and no echo, trading a wrong total for a terminal the user has to `reset` blind.

An external `kill -TERM` — logout, `pkill`, a service manager — still ends the process without
restoring the terminal or blanking the lock. Closing that gap is a post-PoC step:
`signal-hook` forwarding SIGTERM and SIGHUP into the same channel as one more `AppEvent`. Handover
being an event rather than a direct call into teardown is what leaves that open.

## Binary

The binary is **`gng`** — the package stays `Git-N-Grit`, so this needs an explicit stanza,
otherwise the binary inherits the package name:

```toml
[lib]
name = "gng"
path = "src/lib.rs"

[[bin]]
name = "gng"
path = "src/main.rs"
```

The library target is what the integration tests reach: a test in `tests/` links against the crate's
lib, and a binary-only crate exposes nothing to link against. `main.rs` is the entry point over it —
the lock, the second-instance screen, and the run loop.

`gng --storage=<path>` names the storage root, and is the only argument the PoC takes —
repos are registered from inside the TUI, not the CLI. `--storage <path>` reaches the same place, and
anything else is a usage line on stdout and a non-zero exit. Parsing is a walk over
`std::env::args()`; a flag crate would be an eighth dependency carrying one option.

## Dependencies

`ratatui` · `notify` · `serde` (with the `derive` feature) · `serde_json` · `anyhow` · `dirs` ·
`uuid` (with the `v4` feature — it is not on by default)

**crossterm** is not listed separately: it arrives with ratatui and is re-exported as
`ratatui::crossterm`. Declaring it as its own dependency invites linking two incompatible copies.

Errors are opaque `anyhow` values propagated to the top of the loop. Where the UI has to show
one — a rejected path on the add prompt — it renders the error's string rather than matching on
a variant.

`notify`'s newest published version is a `9.0.0-rc`, so pin **8.2** and let `cargo add` pick it.

The `git` binary is a hard runtime requirement, **2.31 or newer** — that is where
`rev-parse --path-format=absolute` arrives, and there is no `--absolute-git-common-dir` to fall back
on. Building needs **Rust 1.89 or newer**, where `File::try_lock` stabilises.

## Tests

The parsers are pure functions over strings — reflog lines, the `--numstat` stream, `patch-id`
output — and the XP rule is arithmetic. Unit-test all of it against fixture text, no git needed.

The rest depends on how git actually behaves, so the scan gets integration tests that build
throwaway repos with `git init` and commit into them for real.

Each such test points `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` at `/dev/null` and passes the four
`GIT_AUTHOR_*` and `GIT_COMMITTER_*` variables to every git call it makes. A real `~/.gitconfig`
otherwise reaches into every throwaway repo: it supplies a `user.email`, which leaves the
no-`user.email` case below unwritable, and a `commit.gpgsign` or a hook there fails the commits
outright. Each test also gets its own storage root, passed in.

- xp arithmetic end to end — a +N −M commit scores `N + 3M`
- a commit authored by another email scores nothing
- a repo with no `user.email` set anywhere scores nothing and says why, rather than reading as idle
- that same repo scanned again with nothing new keeps its `!`, the scan having ended before it could
  ask again
- a scan that fails outright — its git dir replaced by a file — dims the row, leaves the total
  untouched, and puts the error in the right pane
- merge commits are skipped, and a scan following one reaches git not at all
- commit, scan, `--amend`, scan — the second event is a rewrite worth 2
- one scan meeting both a commit and its rewrite for the first time — the original takes full value
  and the rewrite the flat 2, whichever worktree's reflog was read first
- commit, scan, commit, scan — only the new commit is awarded
- scan twice with nothing in between — the second awards nothing and reaches git not at all
- a repo whose reflog has been expired away entirely — a scan awards nothing, rather than HEAD's
  own value
- a commit that scored zero, then rewritten — the rewrite is worth the flat 2
- an `--allow-empty` commit scans cleanly and is not read as a rewrite
- a reflog holding the message-less line `git worktree add` writes parses without error
- scan, `reflog expire --expire=now --all && gc --prune=now`, commit, scan — the total reflects
  only the new work, with no phantom 2 xp for everything that came before
- expire, then commit enough to regrow the reflog past its previous length, then scan — every one
  of those commits is scored, none skipped
- a reflog naming an object gc has pruned still scans cleanly, and that SHA lands in `seen_shas`, so
  the next scan doesn't reach git over it again
- a binary file contributes zero rather than failing to parse, and text in the same commit still
  counts

And on state, identity and storage:

- two registered repos — a commit in one leaves the other's state file byte-identical
- more than 200 commits — `events` holds 200, and `total_xp` still reflects every one of them
- a stray `repos/<id>.json.tmp` is ignored at startup
- an unparseable state file skips that repo and leaves the others loading normally
- a state file numbered ahead of this build skips that repo, and the others load
- `cp` a state file to a second UUID — both rows say the repo appears twice, only the newer earns,
  and deleting either leaves the other earning again
- an unreadable state file gets a row of its own, and `d` on it deletes that file and leaves every
  other one byte-identical
- earn XP, `mv` the repo, re-add — matched by root commit, total unchanged, nothing re-scored
- earn XP, delete the directory, clone a *different* repo to that path, re-add — the picker appears,
  and "new repo" leaves nothing inherited
- earn XP, `rm -rf .git && git init` in place, re-add — the picker appears, and taking the existing
  row keeps its total against the new root commit
- earn XP, delete, re-clone the same upstream to the same path, re-add — XP preserved
- register a repo with no commits, then commit — `root_commits` is adopted on the first scan
- register a repo and one of its linked worktrees — one state file, two `worktrees` entries, one total
- `git worktree add`, then scan — the base commit it seeds the new reflog with earns nothing, having
  already been scored in the main worktree
- commit in a worktree, scan, fast-forward merge into main, scan — the branch tip is scored once, not
  twice, even though it appears in both reflogs
- `git checkout --orphan` a branch, commit, re-add the repo — it matches on the root-commit
  intersection instead of reading as a new repo
- `mv` a registered repo without re-adding it — it loads stale, keeps its total, and is not watched
- `mv` it away, `r`, `mv` it back, `r` — the row un-stales and scores a commit made while it was
  gone, so the watch was registered again and not merely the marker cleared
- `mv` it away, clone a different repo to the path it left, `r` — the row stays stale and inherits
  nothing
- `rm -rf .git && git init` in a registered repo, then commit — the commits score nowhere, and the
  row says the repo it recorded is gone rather than banking them into that total
- `rebase --root` a registered repo, then commit — the commits score nowhere and the total is kept,
  the row saying the history was rewritten rather than that a different repo is there; adding it
  again takes the picker's existing row and it earns from the new history
- `mv` it away and back with nothing else happening — it un-stales on its own within a couple of
  seconds and scores a commit made while it was gone
- a `git clone` in flight into the path a stale repo left does not un-stale that row; a clone of the
  same upstream there does, once its refs are written
- delete a linked worktree's directory by hand — the repo stays live, keeps scoring from its other
  worktrees, and its total is unchanged
- delete the working tree of a `--separate-git-dir` repo — it goes on scoring from the store itself
  rather than failing every scan
- two clones of one upstream, both moved, then one re-added — the picker lists both candidates
- `d` a repo — its state file is gone, every other one is byte-identical, and its watches are dropped

And on running twice:

- an unrecognised argument — usage on stdout, non-zero exit, no TUI and no storage directory created
- start a second instance with stdin not a terminal — it reports the holder on stdout and exits
  non-zero, the holder keeps running, and no state file is touched
- hand over — the holder exits 0 with `.take` blanked, the asker ends up holding it, and the holder's
  state file is intact
- hand over to a holder that cannot answer (`kill -STOP` it first) — the asker times out, exits
  non-zero, `.takeover` is gone, and the holder still holds the lock
- SIGKILL the holder, then start again — the lock is acquired with no cleanup step, and the stale
  identity line is overwritten rather than believed
- a `.take` holding a malformed line with the lock free — the retry acquires it, overwrites the line,
  and starts normally
- a `.take` holding a malformed line with the lock genuinely held — reported on stdout, exits
  non-zero, starts no TUI
- hand over mid-scan — `<id>.json` parses afterwards, holding either the pre-scan or the post-scan
  total, never a partial write

The expire, gc and identity cases matter most: those failures report no error, and surface long
after the commits they lost.

One thing is checked by hand rather than in a test: that the first scan of a repo with a long reflog
leaves the keys responsive, and that the row holds its previous total until the findings land.

## Beyond the PoC — storage

When the data outgrows "load all of it into memory at startup", the answer is **SQLite**, and the
JSON above is deliberately shaped to mirror its schema:

```sql
CREATE TABLE repo(id INTEGER PRIMARY KEY, git_common_dir TEXT,
                  root_commits TEXT, total_xp INTEGER, added_at INTEGER);
CREATE TABLE worktree(repo_id INT, head_subdir TEXT, source_dir TEXT,
                      PRIMARY KEY(repo_id, head_subdir)) WITHOUT ROWID;
CREATE TABLE seen_sha(repo_id INT, sha TEXT, PRIMARY KEY(repo_id, sha)) WITHOUT ROWID;
CREATE TABLE seen_patch_id(repo_id INT, patch_id TEXT, PRIMARY KEY(repo_id, patch_id)) WITHOUT ROWID;
CREATE TABLE event(repo_id INT, sha TEXT, timestamp INT, subject TEXT,
                   added INT, deleted INT, xp INT, rewrite INT);
```

It retires four things at once. A scan becomes one transaction of `INSERT OR IGNORE` per SHA scored
plus `UPDATE repo SET total_xp = total_xp + ?`, so write cost tracks *new* commits instead of
accumulated history — a better fix than per-repo state files, which only address the cross-repo half.
Crash safety comes from the transaction, so the tmp-and-rename goes away. The enumerate-and-sort load
goes away. The events cap becomes `DELETE … ORDER BY timestamp LIMIT`.

It is not in the PoC because it adds `rusqlite` — bundled, compiling SQLite from C — plus a schema
and migrations, to a project whose stated ethos is seven dependencies and parsing `git` stdout. At
~430 kB per repo-year, the whole dataset fits in RAM with room to spare, which is the condition
under which a document format is the right shape.

The upgrade stays embedded. A database server would make a running daemon a prerequisite for a TUI
that counts your own commits, and its durability is the same write-then-flip-a-pointer trick as
`rename(2)` behind a wire protocol.

## Out of scope

Levels · streaks · badges · quests · XP for anything but commits (no rebase, bisect or
conflict-resolution rewards) · staging or committing from inside the TUI · branches beyond what
HEAD's reflog saw · co-author credit · config file · themes.
