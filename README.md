# Git-N-Grit

A terminal UI that watches your Git activity and turns your commits into XP.

Use Git exactly as you do now - Git-N-Grit notices work on your added repositories, and summarises it for you.

## XP

Your commits earn 1 XP per line added and 3 per line deleted.
Redoing work you already earned XP for, such as amending, rebasing or cherry-picking, earns a flat 2 XP.
A commit is yours when its author email matches the repository's `user.email`.

## Requirements

- Linux (other platforms are untested)
- Git 2.31 or newer
- Rust 1.89 or newer, to build from source

## Install

```sh
make install
```

This builds `gng` and copies it to `~/.local/bin`, adding that directory to your `PATH` if needed.

## Usage

```sh
gng
gng --storage <path>   # use state-tracking files somewhere other than ~/.local/share/git-n-grit
```

Tracked repositories are added from inside the app.

Technical decisions are documented in [docs/DECISIONS.md](docs/DECISIONS.md).
