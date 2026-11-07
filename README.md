# Git-N-Grit

Gamify your software development!

Git-N-Grit is a terminal app that keeps track of your Git commits, turning your work into XP.  
Keep using Git as you usually do: the app only watches the repos you add, and never changes them.

<!-- preview:start -->
```text
┌ Git-N-Grit ──────────────────────┐┌──────────────────────────────────────────┐
│  + Add repo…                     ││/home/you/code/api          total 280xp   │
│> api 280xp                       ││6m ago  docs: explain scoring             │
│  dotfiles 147xp                  ││          +14 −0  +14xp                   │
│  notes 12xp                      ││38m ago  refactor: drop the old cache     │
│                                  ││          +0 −34  +102xp                  │
│                                  ││1h ago  fix: scroll off-by-one (redo)     │
│                                  ││          rewritten  +2xp                 │
│                                  ││2h ago  fix: scroll bug                   │
│                                  ││          +2 −0  +2xp                     │
│                                  ││3h ago  feat: parse reflog                │
│                                  ││          +120 −0  +120xp                 │
│                                  ││5h ago  Initial commit                    │
│                                  ││          +40 −0  +40xp                   │
│                                  ││                                          │
└──────────────────────────────────┘└──────────────────────────────────────────┘
  ↑↓/jk move   enter select   a add   d remove
  r refresh   PgUp/PgDn scroll   home newest   q quit
```
<!-- preview:end -->

## XP

Your commits earn XP:

- Deleting code is worth more than writing it: 1 XP per added line, 3 XP per deleted line.
- Redone work (amends, rebases, cherry-picks) only counts once, and the cleanup earns a flat 2 XP.

A commit is yours when its author email matches the repo's Git `user.email`.

## Requirements

- Linux (other platforms are untested)
- Git 2.31 or newer
- Rust 1.89 or newer (to build from source)

## Install

```sh
git clone https://github.com/EvAvKein/Git-N-Grit.git
cd Git-N-Grit
make install
```

This builds `gng` and copies it to `~/.local/bin`, adding that directory to your `PATH` if needed.

## Usage

```sh
gng
gng --storage <path>   # keep Git-N-Grit's data somewhere other than ~/.local/share/git-n-grit
```

Technical and design details, like how it keeps a rebase from rewarding the same work twice, can be found in [docs/DECISIONS.md](docs/DECISIONS.md).
