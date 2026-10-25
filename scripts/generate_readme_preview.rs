//! `make readme-preview`: draw one frame of the real UI over throwaway repos and
//! print it as the README's preview block, or with `--write`, put it into the
//! README between the preview markers.

#[path = "../tests/common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use common::{Fixture, MINE, Repo, THEIRS};
use gng::controller::Controller;
use gng::model::event::AppEvent;
use gng::model::scan::{apply, run_scan};
use gng::model::storage::Storage;
use gng::view::{self, Palette};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

const START: &str = "<!-- preview:start -->";
const END: &str = "<!-- preview:end -->";
const WIDTH: u16 = 80;
const HEIGHT: u16 = 18;

fn main() -> Result<()> {
    let write = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--write" => true,
        _ => bail!("usage: generate_readme_preview [--write]"),
    };
    let frame = render()?;
    if !write {
        print!("{}", block(&frame));
        return Ok(());
    }
    let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let text = fs::read_to_string(&readme).context("reading README.md")?;
    fs::write(&readme, splice(&text, &frame)?).context("writing README.md")
}

fn render() -> Result<String> {
    let fixture = Fixture::new();
    let storage = Storage::new(fixture.path("storage"));
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    // Half a minute past each mark, so a slow run still reads the same.
    let ago = |minutes: i64| now - minutes * 60 - 30;

    let api = fixture.repo("api");
    commit(
        &api,
        MINE,
        ago(310),
        "Initial commit",
        &[("src/main.rs", 40)],
    );
    commit(
        &api,
        MINE,
        ago(200),
        "feat: parse reflog",
        &[("src/reflog.rs", 120)],
    );
    commit(
        &api,
        MINE,
        ago(125),
        "fix: scroll bug",
        &[("src/main.rs", 42)],
    );
    // Reworded only, so its patch-id is already paid.
    api.run_at(
        MINE,
        ago(110),
        &["commit", "--amend", "-m", "fix: scroll off-by-one"],
    );
    commit(
        &api,
        THEIRS,
        ago(52),
        "chore: bump deps",
        &[("Cargo.toml", 8)],
    );
    commit(
        &api,
        MINE,
        ago(38),
        "refactor: drop the old cache",
        &[("src/reflog.rs", 86)],
    );
    commit(
        &api,
        MINE,
        ago(6),
        "docs: explain scoring",
        &[("README.md", 14)],
    );

    let dotfiles = fixture.repo("dotfiles");
    commit(&dotfiles, MINE, ago(900), "Add zshrc", &[("zshrc", 60)]);
    commit(&dotfiles, MINE, ago(700), "Trim aliases", &[("zshrc", 31)]);

    let notes = fixture.repo("notes");
    commit(&notes, MINE, ago(1200), "Start notes", &[("todo.md", 12)]);

    for (i, (repo, shown)) in [
        (&api, "/home/you/code/api"),
        (&dotfiles, "/home/you/dotfiles"),
        (&notes, "/home/you/notes"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut state = repo.state();
        let findings = run_scan(&state)?;
        apply(&mut state, findings);
        // The left pane's order, and a path that reads the same on every machine.
        state.added_at = i as i64;
        state.worktrees[0].source_dir = PathBuf::from(shown);
        storage.save(&state)?;
    }

    // Drawn before the startup scans answer, as the app's own first frame is.
    let (outbox, _inbox) = mpsc::channel();
    let mut controller = Controller::new(storage, outbox)?;
    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    controller.handle(AppEvent::Input(Event::Key(down)))?;

    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))?;
    terminal.draw(|frame| view::draw(frame, &mut controller, 0, Palette::plain()))?;
    let buffer = terminal.backend().buffer();
    let lines: Vec<String> = (0..HEIGHT)
        .map(|y| {
            let line: String = (0..WIDTH).map(|x| buffer[(x, y)].symbol()).collect();
            line.trim_end().to_string()
        })
        .collect();
    Ok(lines.join("\n"))
}

/// One commit writing `lines` numbered lines to each file.
fn commit(repo: &Repo, author: &str, at: i64, message: &str, files: &[(&str, usize)]) {
    for (name, lines) in files {
        let path = repo.dir.join(name);
        fs::create_dir_all(path.parent().expect("a file under the repo")).expect("creating a dir");
        let text: String = (1..=*lines).map(|i| format!("line {i}\n")).collect();
        repo.write(name, &text);
    }
    repo.run(&["add", "-A"]);
    repo.run_at(author, at, &["commit", "-m", message]);
}

/// `readme` with what lies between the markers replaced by `frame`.
fn splice(readme: &str, frame: &str) -> Result<String> {
    let (Some(start), Some(end)) = (readme.find(START), readme.find(END)) else {
        bail!("README.md has no {START} … {END} markers");
    };
    ensure!(start < end, "README.md has {END} before {START}");
    let head = &readme[..start + START.len()];
    Ok(format!("{head}\n{}{}", block(frame), &readme[end..]))
}

/// What goes between the markers.
fn block(frame: &str) -> String {
    format!("```text\n{frame}\n```\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_replaces_what_lies_between_the_markers() {
        let readme = format!("# x\n\n{START}\nold\n{END}\n\n## y\n");
        assert_eq!(
            splice(&readme, "new").unwrap(),
            format!("# x\n\n{START}\n```text\nnew\n```\n{END}\n\n## y\n")
        );
    }

    #[test]
    fn a_second_run_changes_nothing() {
        let once = splice(&format!("{START}\n{END}\n"), "frame").unwrap();
        assert_eq!(splice(&once, "frame").unwrap(), once);
    }

    #[test]
    fn missing_or_reversed_markers_are_an_error() {
        assert!(splice("# x\n", "f").is_err());
        assert!(splice(&format!("{END}\n{START}\n"), "f").is_err());
    }
}
