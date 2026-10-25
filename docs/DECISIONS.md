# Git-N-Grit — Decisions

Why Git-N-Grit works the way it does. Each section is a choice and the failure it prevents. The git
behavior described here was checked against git 2.47.3.

## Read the reflog, touch nothing

Git-N-Grit reads `logs/HEAD` and runs read-only `git` commands. It never writes to a repo, so you
keep using git however you already do. It shells out to the `git` binary instead of using a
library, which keeps the scoring tied to exactly what git reports.

The reflog is the whole input. It expires on its own, so there's no time window to configure: if git
still remembers a commit, it counts.

## Scoring

- Only commits whose author email matches the repo's `user.email` earn XP. The email is read on
  every scan and never stored, so changing identity needs no migration. A repo with no `user.email`
  gets a `!` saying so; otherwise it would look like a repo where no work was done.
- `xp = added + 3 × deleted`. Deleting code is worth more than writing it.
- A commit whose patch-id was already paid for (an amend, rebase or cherry-pick) earns a flat 2.
  The work counts once, and the cleanup still counts a little.
- Merges score nothing, and only your own commits are listed. Other authors' commits are still
  recorded, so a shared repo doesn't rescan them.

## Two seen-sets, kept forever

Each repo keeps `seen_patch_ids` ("was this work paid for?") and `seen_shas` ("was this commit
processed?"). Without `seen_shas`, every rescan would replay old commits, and each would match its
own patch-id and earn another 2.

Neither set is ever pruned. A forgotten patch-id lets a rewrite earn full value again. A forgotten
SHA pays out a second time when `git checkout` puts an old commit back in the reflog.

`total_xp` is a running sum, never recomputed from `events`. So `events` can be capped at the newest
200 without changing the total.

## Scanning

A scan reads every reflog whole, drops SHAs already in `seen_shas`, and puts what's left through two
batched calls: `git log --numstat` for stats and `git log -p | git patch-id --stable` for
patch-ids. In steady state that's one new commit, and a scan with nothing new runs no git at all.

The calls depend on git details that fail quietly if mishandled:

- **Empty input falls back to HEAD.** Handed nothing on stdin, `git log --stdin` scores HEAD,
  a commit no reflog named. So a scan with nothing left stops before calling git.
- **`--ignore-missing` must come before `--stdin`.** Placed after, it's accepted and does nothing,
  and a single pruned object aborts the scan with `fatal: bad object`. A SHA that git drops this way
  is still recorded as seen, so it isn't retried until its reflog entry expires.
- **Score in committer-date order.** A rewrite has to meet its original already paid, or the flat 2
  goes to the wrong one of the pair. Reflogs pooled from several worktrees aren't chronological, and
  a repo's first scan sees both commits at once.
- **Git runs in a directory that still exists.** A worktree's git dir can outlive its files, and
  `git -C` on a missing directory fails the whole scan. Git runs in the first worktree whose files
  remain, otherwise in the git dir itself.

Every `git` call and every `stat` runs on one worker thread, never the main thread. A path on a dead
mount blocks whatever touches it, and the main thread has to keep drawing and answering `q`.

## One state file per object store

State lives in `~/.local/share/git-n-grit/repos/`, one JSON file per object store, named by a
random UUID. A repo and all its linked worktrees share one file. `git worktree add` seeds the new
reflog with an already-scored commit, and a fast-forward merge puts a branch tip into two reflogs.
Separate seen-sets per worktree would pay twice for both.

A file per store keeps writes small (a scan fires on every commit), and a bad write costs one repo,
not all of them. Writes go to a temp file that is then renamed over the target. An in-place write
crashing midway would leave invalid JSON, losing `seen_patch_ids` and making every commit in the
reflog earn again.

A state file that won't load gets a row of its own with the reason, rather than the repo vanishing,
which would look like lost XP. A file from a newer format version doesn't load: saving it would drop
the fields this build doesn't know.

## Identity by root commit, not path

Paths move and get reused, and a path-keyed scheme would get both wrong without a sign. A moved repo
brings its reflog along and would earn everything again. A new project cloned into an old path
would inherit the old total. So a state file is matched by its root commits, plus its stored
`.git` directory where that still exists.

Matching takes any shared root, not an identical set. `git checkout --orphan` adds a second root,
and an exact match would treat the repo as new and score its reflog again.

| Situation | Result |
|---|---|
| `mv` the repo, then add it again | same row, nothing scored twice |
| re-clone the same upstream in place | same row, XP kept |
| a different project cloned to the old path | you pick; "new repo" inherits nothing |
| `rm -rf .git && git init` in place | you pick; the old row keeps its total |
| `rebase --root` or amending the first commit | you pick; the old row keeps its total |
| two clones of one upstream, both moved | you pick between them |

Where git can't tell cases apart, you pick. Guessing either merges two histories or rescores
a whole reflog at full value.

A path refilled between scans looks identical to a `stat`, so before scoring any new commit a scan
checks that the repo there still has a root commit the state file holds.

## Stale repos and dead mounts

A repo whose `.git` directory is gone stays in the list, dimmed with a `!` and its total kept. Left
unmarked, it would look idle while earning nothing.

A once-a-second probe notices when it returns, since a returning mount or a directory moved back
fires no event. It counts as back only once its root commits show again. `git clone` creates `.git`
before writing any ref, so an empty answer means "still being written", not "match".

A `stat` on a dead NFS or FUSE mount can block forever, and nothing can cancel the thread. So each
filesystem-touching thread records what it's inside. After ten seconds the app abandons it, starts a
replacement that skips that path, and labels the row "filesystem is not responding".

## Watching for commits

The watch is on each worktree's `logs` directory, not on `logs/HEAD`. Expire and gc rename a new
file over `HEAD`, which would leave a file watch on a dead inode and miss every later commit without
any error.

A watch follows an inode, so renaming a worktree's directory tells its own watch nothing. Each
worktree's parent directory is watched too: a rename within the parent is followed, and a move
elsewhere dims the row.

Keys, file events, scan results and returning repos all come down one channel to the main thread.
With nothing in flight, it blocks and wakes for nothing.

## One instance at a time

Two instances would each rewrite whole state files, and the results couldn't be merged afterwards:
of two diverging running totals, one is wrong. So `gng` holds an `flock` on
`~/.local/share/git-n-grit/.take` while it runs. The kernel drops it when the process dies, so
there's never a stale lock to clean up.

A second `gng` offers to close or take over. Taking over writes a `.takeover` file, which the first
instance's watcher sees and answers by quitting normally. It never kills the other instance: a
killed TUI leaves the text cursor invisible until a `reset`. An outside `kill -TERM` has that
effect too.

## Smaller choices

- **No mouse capture**, so selecting text works as in any terminal.
- **Color never carries meaning alone**, for accessibility. Dimming, the `!` and the reason text
  cover every state, and `NO_COLOR` turns color off.
- **Times use the local day**, from the offset in `git var GIT_COMMITTER_IDENT`. `std` only knows
  UTC, which would flip "yest." hours into the local day.
- **The storage root is passed down, never looked up**, so each test gets its own.
  `std::env::set_var` is `unsafe` in edition 2024 and races other tests.
- **Seven dependencies**, and no argument-parsing crate for one flag.
- **git 2.31+**, for `rev-parse --path-format=absolute`. **Rust 1.89+**, for `File::try_lock`.

## Tests

Integration tests build real throwaway repos and point git's global and system config at
`/dev/null`. Otherwise a developer's `~/.gitconfig` would supply a `user.email` or a signing hook.

The expire, gc and identity cases matter most: those failures report no error and turn up long after
the commits they lost.
