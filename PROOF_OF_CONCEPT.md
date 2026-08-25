# Git-N-Grit — Proof of Concept

A Ratatui TUI that watches your git reflog and turns your own commits into XP.

You keep using git exactly as you do now, in whatever terminal or editor you like. Git-N-Grit
just sits there, notices the work, and reflects it back at you. It never touches your repo.

Scope of this document is the **first working version only**. Levels, streaks and badges are
deliberately absent — see [Out of scope](#out-of-scope).

## Screen

```
┌ Git-N-Grit ────────────────────┬──────────────────────────────────┐
│ + Add repo…                    │ Git-N-Grit          total 1,240xp│
│ > Git-N-Grit          1,240xp  │                                  │
│   dotfiles              380xp  │  2h ago  feat: parse reflog      │
│   work/api            4,102xp  │          +120 −34         +222xp │
│                                │  5h ago  fix: off-by-one         │
│                                │          +3 −3             +12xp │
│                                │  yest.   refactor: drop legacy   │
│                                │          +12 −240         +732xp │
│                                │  yest.   fix: off-by-one  (redo) │
│                                │          rewritten          +2xp │
└────────────────────────────────┴──────────────────────────────────┘
  ↑↓/jk move   enter select   a add repo   r refresh   q quit
```

Left: the repos you've registered, each with its total. Right: the selected repo's recent
activity, newest first.

## XP rules

- Only commits whose **author email** matches that repo's `git config user.email` earn anything.
- `xp = lines_added × 1 + lines_deleted × 3`. Deleting code is worth more than writing it.
- A commit whose patch-id has already been recorded — a rebase, amend or cherry-pick of work
  you were already paid for — earns a flat **2 xp** instead. The work counts once; the cleanup
  still counts a little. Patch-ids are tracked per repo.
- Merge commits are skipped.

## How it reads git

Shells out to the `git` binary and parses stdout. No git library.

**1 — candidates.** Read `.git/logs/HEAD` and collect the "new" SHA (field 2) of every line.
That's the whole input set, and it's naturally bounded by the reflog's own expiry, so there is
no window setting to pick: if git still remembers it, it counts.

**2 — stats.** Pipe those SHAs into one batched call:

```sh
git log --no-walk=unsorted --stdin --numstat --format='C%x00%H%x00%ae%x00%ct%x00%p%x00%s'
```

giving author email, timestamp, subject, parents (empty or one = not a merge) and the
added/deleted counts per commit.

**3 — patch-ids.** One more batched call, emitting `<patch-id> <sha>` per commit:

```sh
git log --no-walk --stdin -p | git patch-id --stable
```

**Incremental.** Store the byte length of `.git/logs/HEAD` as of the last scan and read only
the tail next time. If the file is *shorter* than the stored offset, gc pruned it — re-read the
whole thing.

## State

One global file at `dirs::data_dir()/git-n-grit/state.json` — `~/.local/share/git-n-grit/state.json`
on Linux — keyed by repo path:

```json
{
  "version": 1,
  "repos": {
    "/home/eve/repo/Git-N-Grit": {
      "total_xp": 1240,
      "reflog_offset": 8814,
      "seen_patch_ids": ["84b8408…"],
      "events": [
        { "sha": "a0f0365…", "ts": 1756100000, "subject": "feat: parse reflog",
          "added": 120, "deleted": 34, "xp": 222, "rewrite": false }
      ]
    }
  }
}
```

## Adding a repo

`a` opens an inline path prompt. The path is accepted only if `git -C <path> rev-parse --git-dir`
succeeds, so worktrees and `.git`-as-a-file both work. A rejected path shows an inline error and
leaves the prompt open.

## Liveness

One `notify` watcher across the `.git/logs/HEAD` of every registered repo. A write triggers a
rescan of that repo and a redraw — commit in another pane and the XP appears on its own. `r`
forces a manual rescan.

Both input and file events funnel into a single `std::sync::mpsc` channel, so the main thread
blocks on `recv()` and never polls:

```
thread:  loop { event::read() }   ──┐
                                    ├──> mpsc::Sender<AppEvent> ──> main: rx.recv()
notify's own watcher thread       ──┘
```

`notify` already sends into a channel of your choosing, so the only thread this project spawns
by hand is the key reader.

## Binary

The binary is **`gng`** — the package stays `Git-N-Grit`, so this needs an explicit stanza,
otherwise the binary inherits the package name:

```toml
[[bin]]
name = "gng"
path = "src/main.rs"
```

`gng` takes no arguments in the PoC. Repos are registered from inside the TUI, not the CLI.

## Dependencies

`ratatui` · `notify` · `serde` (with the `derive` feature) · `serde_json` · `anyhow` · `dirs`

One deliberate absence: **crossterm** is not listed because it already arrives with ratatui and
is re-exported as `ratatui::crossterm`. Declaring it separately invites linking two incompatible
copies.

Errors are opaque `anyhow` values propagated to the top of the loop. Where the UI has to show
one — a rejected path on the add prompt — it renders the error's string rather than matching on
a variant.

One version note: `notify`'s newest published version is a `9.0.0-rc`, so pin **8.2** and let
`cargo add` pick it.

The `git` binary is a hard runtime requirement.

## Out of scope

Levels · streaks · badges · quests · XP for anything but commits (no rebase, bisect or
conflict-resolution rewards) · staging or committing from inside the TUI · branches beyond what
HEAD's reflog saw · co-author credit · config file · themes.
