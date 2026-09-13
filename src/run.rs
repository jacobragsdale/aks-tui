//! The terminal loop: take the terminal, draw, read a key, give the terminal
//! back — on every exit path, including a panic.

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser as _;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;

use crate::app::App;
use crate::app::screen::AppAction;
use crate::cli::Cli;
use crate::{clipboard, config, paths, session, ui};

/// How long a settled screen waits for a key before looking at the clock.
const RESTING: Duration = Duration::from_millis(250);
/// How long a layout has to stop changing before it is written. Holding `S`
/// through six columns is one save, not six.
const SETTLE: Duration = Duration::from_millis(500);

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = paths::config_file(cli.config.as_deref());
    let config = cli.merge(config::load(&config_path)?);
    let theme = cli
        .resolve_theme(&config)
        .and_then(|choice| choice.theme(&config))
        .with_context(|| format!("resolving the theme (config: {})", config_path.display()))?;
    ui::theme::set_theme(theme);
    tui(&cli, config)
}

fn tui(_cli: &Cli, config: config::Config) -> Result<()> {
    let mut app = App::new(config.tabs());
    // Before the first frame, so nothing is drawn in a layout that is about
    // to change.
    let session_path = paths::session_file();
    app.restore(&session::Session::load(&session_path));
    let mut saved = serde_json::to_string(&app.session()).unwrap_or_default();
    let mut settling: Option<Instant> = None;
    let started = Instant::now();

    let mut terminal = ratatui::init();
    // From here on the terminal is ours, so every way out of this function —
    // an error, a panic, `q` — goes through the guard's `Drop`.
    let _restore = TerminalRestore;
    enable_terminal_input()?;

    loop {
        terminal
            .draw(|frame| app.render(frame, started.elapsed().as_millis()))
            .context("failed to draw")?;

        if event::poll(RESTING)? {
            let action = match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                // Bracketed paste is on, so a paste arrives whole rather than
                // as keystrokes — and would be dropped here if nothing took it.
                Event::Paste(text) => app.handle_paste(&text),
                _ => AppAction::None,
            };
            if act(&mut app, action) {
                // The layout goes with the run, settle timer or not.
                let _ = app.session().save(&session_path);
                return Ok(());
            }
        }

        // The layout, once it has stopped moving.
        let now = serde_json::to_string(&app.session()).unwrap_or_default();
        if now == saved {
            settling = None;
        } else {
            let due = *settling.get_or_insert_with(|| Instant::now() + SETTLE);
            if Instant::now() >= due {
                settling = None;
                saved = now;
                if let Err(error) = app.session().save(&session_path) {
                    // A layout that will not save is worth saying once.
                    app.shell
                        .set_error(format!("could not save the session: {error:#}"));
                }
            }
        }
    }
}

/// Does what a screen asked for. Returns true when the run is over.
fn act(app: &mut App, action: AppAction) -> bool {
    match action {
        AppAction::Quit => return true,
        AppAction::Copy { text, label } => match clipboard::copy(&text) {
            Ok(clipboard::Channel::Command) => app.shell.set_status(label),
            // The escape went out and nothing confirmed it; a terminal that
            // does not speak it has dropped the text, so say which it was.
            Ok(clipboard::Channel::Terminal) => app
                .shell
                .set_status(format!("{label} · sent to the terminal (OSC 52)")),
            Err(error) => app.shell.set_error(format!("{error:#}")),
        },
        AppAction::None => {}
    }
    false
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        // Best effort: the run is over either way, and a terminal that
        // refuses one of these is not something the exit can fix.
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        ratatui::restore();
    }
}

/// The input the TUI reads beyond the keyboard. Turned on here so no later
/// step has to remember to.
fn enable_terminal_input() -> Result<()> {
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)
        .context("failed to enable terminal input features")
}
