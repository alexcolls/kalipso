mod app;
mod chat;
mod editor;
mod files;
mod git;
mod llm;
mod lsp;
mod term;
mod tools;
mod vendors;

use std::io::{self, IsTerminal, stdout};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::style::Print;

use app::{App, Event};

/// Tells terminals that support it (WezTerm) that Kalipso is running, so a config can
/// pass Alt+arrows and Alt+1-4 through to us. The value is base64 for "1". See README.
const USER_VAR_SET: &str = "\x1b]1337;SetUserVar=KALIPSO=MQ==\x07";
const USER_VAR_CLEAR: &str = "\x1b]1337;SetUserVar=KALIPSO=\x07";

const FRAME: Duration = Duration::from_millis(16);
/// How long one batch may drain events before a frame gets a chance to draw.
const DRAIN_BUDGET: Duration = Duration::from_millis(8);
/// Bounded so a flood like `yes` blocks the reader instead of growing memory. Keys wait
/// behind whatever is queued, so this also caps input lag during a flood.
const CHANNEL_SIZE: usize = 32;

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    let root = PathBuf::from(arg.as_deref().unwrap_or("."));
    let root = match root.canonicalize() {
        Ok(root) if root.is_dir() => root,
        _ => {
            eprintln!("usage: kalipso-cli [directory]");
            return ExitCode::from(2);
        }
    };
    if !stdout().is_terminal() {
        eprintln!("kalipso-cli must run in a terminal");
        return ExitCode::from(2);
    }

    // ratatui's own hook restores raw mode and the alternate screen, then calls this one.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = release_terminal();
        hook(info);
    }));

    let result = ratatui::run(|terminal| {
        execute!(
            stdout(),
            EnableMouseCapture,
            EnableBracketedPaste,
            Print(USER_VAR_SET)
        )?;
        let result = run(terminal, root);
        release_terminal()?;
        result
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("kalipso-cli: {err}");
            ExitCode::FAILURE
        }
    }
}

fn release_terminal() -> io::Result<()> {
    execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        Print(USER_VAR_CLEAR)
    )
}

fn run(terminal: &mut ratatui::DefaultTerminal, root: PathBuf) -> io::Result<()> {
    let (tx, rx) = mpsc::sync_channel(CHANNEL_SIZE);

    let input_tx = tx.clone();
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if input_tx.send(Event::Input(event)).is_err() {
                break;
            }
        }
    });
    let tick_tx = tx.clone();
    thread::spawn(move || {
        while tick_tx.send(Event::Tick).is_ok() {
            thread::sleep(Duration::from_secs(1));
        }
    });

    thread::spawn(editor::syntax); // Slow to load, so have it ready before the first file opens.

    let mut app = App::new(root, tx);
    let mut next_frame = Instant::now();
    loop {
        let first = if app.needs_draw {
            rx.recv_timeout(next_frame.saturating_duration_since(Instant::now()))
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match first {
            Ok(event) => {
                app.handle(event);
                let budget = Instant::now() + DRAIN_BUDGET;
                while Instant::now() < budget {
                    let Ok(event) = rx.try_recv() else { break };
                    app.handle(event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if app.should_quit {
            break;
        }
        if app.needs_draw && Instant::now() >= next_frame {
            terminal.draw(|frame| app.draw(frame))?;
            app.needs_draw = false;
            next_frame = Instant::now() + FRAME;
        }
    }
    Ok(())
}
