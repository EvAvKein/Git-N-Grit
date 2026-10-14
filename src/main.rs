use std::fs;
use std::io::IsTerminal;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use gng::controller::Controller;
use gng::event::AppEvent;
use gng::git::local_utc_offset;
use gng::lock::{self, Acquired, Holder, Lock};
use gng::state::Storage;
use gng::view::{self, Palette, TakeoverScreen};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

const USAGE: &str = "usage: gng [--storage=<path>]";

fn main() -> ExitCode {
    let storage = match parse_args(std::env::args().skip(1)) {
        Ok(Some(root)) => Storage::new(root),
        Ok(None) => match dirs::data_dir() {
            Some(dir) => Storage::new(dir.join("git-n-grit")),
            None => {
                println!("gng: no data directory to keep state in; pass --storage=<path>");
                return ExitCode::FAILURE;
            }
        },
        Err(()) => {
            println!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match launch(storage) {
        Ok(code) => code,
        Err(err) => {
            println!("gng: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `Err` is a usage error; `None` is no `--storage` given.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<PathBuf>, ()> {
    let mut args = args;
    let mut root = None;
    while let Some(arg) = args.next() {
        let value = if let Some(value) = arg.strip_prefix("--storage=") {
            value.to_string()
        } else if arg == "--storage" {
            args.next().ok_or(())?
        } else {
            return Err(());
        };
        if value.is_empty() || root.replace(PathBuf::from(value)).is_some() {
            return Err(());
        }
    }
    Ok(root)
}

fn launch(storage: Storage) -> Result<ExitCode> {
    fs::create_dir_all(storage.root())
        .with_context(|| format!("creating {}", storage.root().display()))?;
    let offset = local_utc_offset();
    let palette = Palette::detect();

    let (lock, mut terminal) = match lock::acquire(&storage)? {
        Acquired::Held(lock) => (lock, ratatui::try_init().context("starting the terminal")?),
        Acquired::Busy(holder) => match offer_takeover(&storage, holder, offset, palette)? {
            ControlFlow::Continue(held) => held,
            ControlFlow::Break(code) => return Ok(code),
        },
    };

    // No early return from here: every exit, error included, blanks the lock
    // and gives the terminal back.
    let result = run_app(storage, &mut terminal, offset, palette);
    lock.release();
    ratatui::restore();
    result.map(|()| ExitCode::SUCCESS)
}

/// `Continue` with the lock taken over; `Break` with the code to exit with.
fn offer_takeover(
    storage: &Storage,
    holder: Holder,
    offset: i32,
    palette: Palette,
) -> Result<ControlFlow<ExitCode, (Lock, DefaultTerminal)>> {
    let described = holder.describe(offset);
    if !std::io::stdin().is_terminal() {
        println!("Git-N-Grit is already open in another terminal: {described}");
        return Ok(ControlFlow::Break(ExitCode::FAILURE));
    }

    let mut terminal = ratatui::try_init().context("starting the terminal")?;
    let move_here = match ask_move_here(&mut terminal, &described, palette) {
        Ok(choice) => choice,
        Err(err) => {
            ratatui::restore();
            return Err(err);
        }
    };
    if !move_here {
        ratatui::restore();
        return Ok(ControlFlow::Break(ExitCode::SUCCESS));
    }

    let waiting = terminal
        .draw(|frame| view::draw_takeover(frame, &described, &TakeoverScreen::Waiting, palette));
    let taken = waiting
        .map_err(anyhow::Error::from)
        .and_then(|_| lock::take_over(storage));
    match taken {
        Ok(Some(lock)) => Ok(ControlFlow::Continue((lock, terminal))),
        Ok(None) => {
            ratatui::restore();
            println!("The other Git-N-Grit didn't answer and is still running: {described}");
            Ok(ControlFlow::Break(ExitCode::FAILURE))
        }
        Err(err) => {
            ratatui::restore();
            Err(err)
        }
    }
}

/// `true` for "Move it here". `q`, esc and ctrl+c close this one.
fn ask_move_here(
    terminal: &mut DefaultTerminal,
    described: &str,
    palette: Palette,
) -> Result<bool> {
    let mut move_here = false;
    loop {
        terminal.draw(|frame| {
            view::draw_takeover(
                frame,
                described,
                &TakeoverScreen::Choose { move_here },
                palette,
            )
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(false);
        }
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char('k' | 'j') => move_here = !move_here,
            KeyCode::Enter => return Ok(move_here),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
            _ => {}
        }
    }
}

fn run_app(
    storage: Storage,
    terminal: &mut DefaultTerminal,
    offset: i32,
    palette: Palette,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();

    let keys = tx.clone();
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if keys.send(AppEvent::Input(event)).is_err() {
                break;
            }
        }
    });

    let mut controller = Controller::new(storage, tx)?;
    controller.run(&rx, |controller| {
        terminal.draw(|frame| view::draw(frame, controller, offset, palette))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<PathBuf>, ()> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn no_arguments_leave_the_root_to_the_default() {
        assert_eq!(parse(&[]), Ok(None));
    }

    #[test]
    fn storage_takes_either_spelling() {
        assert_eq!(parse(&["--storage=/x"]), Ok(Some("/x".into())));
        assert_eq!(parse(&["--storage", "/x"]), Ok(Some("/x".into())));
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        for args in [
            &["--bogus"][..],
            &["/x"],
            &["--storage"],
            &["--storage="],
            &["--storage=/a", "--storage=/b"],
        ] {
            assert_eq!(parse(args), Err(()), "{args:?}");
        }
    }
}
