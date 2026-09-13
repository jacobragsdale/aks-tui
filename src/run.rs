//! The terminal loop: take the terminal, start the worker, draw, read a key,
//! drain the worker, give the terminal back — on every exit path, including
//! a panic.

use std::io::{self, Write as _};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser as _;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

use crate::app::App;
use crate::app::screen::AppAction;
use crate::cli::{Cli, Command as Subcommand};
use crate::kube::{self, Handle, Kubectl, Request};
use crate::store::Store;
use crate::{cache, clipboard, config, doctor, paths, session, ui};

/// How long a settled screen waits for a key before looking at the clock.
const RESTING: Duration = Duration::from_millis(250);
/// How long a layout has to stop changing before it is written. Holding `S`
/// through six columns is one save, not six.
const SETTLE: Duration = Duration::from_millis(500);
/// How often the cache is rewritten while reads keep landing. Every read
/// would be a file write every few seconds for nothing anyone can see.
const CACHE_EVERY: Duration = Duration::from_secs(30);

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = paths::config_file(cli.config.as_deref());
    let config = cli.merge(config::load(&config_path)?);
    let theme = cli
        .resolve_theme(&config)
        .and_then(|choice| choice.theme(&config))
        .with_context(|| format!("resolving the theme (config: {})", config_path.display()))?;
    ui::theme::set_theme(theme);
    match cli.command {
        Some(Subcommand::Doctor) => {
            let mut out = io::stdout().lock();
            let ok = doctor::doctor(&mut out, &config)?;
            out.flush()?;
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Subcommand::Setup { write }) => {
            let mut out = io::stdout().lock();
            let ok = doctor::setup(&mut out, write, &config_path)?;
            out.flush()?;
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        None => tui(&cli, config),
    }
}

fn tui(cli: &Cli, config: config::Config) -> Result<()> {
    let tabs = config.tabs();
    // The cache is read before the terminal is taken, so the first frame is
    // painted from it rather than after it.
    let cache_path = paths::cache_file(cli.cache.as_deref());
    let store = match (cli.no_cache, cache::load(&cache_path)) {
        (false, Some(snapshot)) => Store::from_cache(&snapshot, &tabs),
        _ => Store::new(tabs.len()),
    };
    let fast = config
        .refresh
        .map_or(kube::DEFAULT_REFRESH, Duration::from_secs);
    let scopes = tabs.iter().map(|tab| tab.scope.clone()).collect();
    let worker = Handle::spawn(Box::new(Kubectl), scopes, fast)?;

    let mut app = App::new(tabs, store);
    // Before the first frame, so nothing is drawn in a layout that is about
    // to change — and so the worker reads the tab that will actually show.
    let session_path = paths::session_file();
    app.restore(&session::Session::load(&session_path));
    if !app.tabs.is_empty() {
        worker.send(Request::Showing(app.tab, app.kind()))?;
    }
    let mut saved = serde_json::to_string(&app.session()).unwrap_or_default();
    let mut settling: Option<Instant> = None;
    let mut cache_written = Instant::now();
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

        if event::poll(app.poll_for(RESTING))? {
            let action = match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                // Bracketed paste is on, so a paste arrives whole rather than
                // as keystrokes — and would be dropped here if nothing took it.
                Event::Paste(text) => app.handle_paste(&text),
                _ => AppAction::None,
            };
            match act(&mut app, &worker, action) {
                Outcome::Quit => {
                    // The layout and the cache go with the run, timers or not.
                    let _ = app.session().save(&session_path);
                    if app.cache_dirty && !cli.no_cache {
                        let _ = cache::save(&cache_path, &app.store.snapshot(&app.tabs));
                    }
                    return Ok(());
                }
                // Whatever ratatui thought was on screen died with the frame
                // the shell drew over.
                Outcome::Repaint => terminal.clear().context("failed to repaint")?,
                Outcome::Continue => {}
            }
        }

        // Everything the worker has said since the last frame.
        while let Some(event) = worker.try_event() {
            if matches!(event, kube::Event::Stopped) {
                app.shell
                    .set_error("the cluster worker stopped; restart aks-tui");
            }
            app.apply(event);
        }
        // Whatever the pane should be following now, and the owner of the
        // pod the cursor has settled on.
        for request in app.tick(Instant::now()) {
            if let Err(error) = worker.send(request) {
                app.shell.set_error(format!("{error:#}"));
            }
        }
        if app.cache_dirty && !cli.no_cache && cache_written.elapsed() >= CACHE_EVERY {
            cache_written = Instant::now();
            app.cache_dirty = false;
            if let Err(error) = cache::save(&cache_path, &app.store.snapshot(&app.tabs)) {
                // A cache that will not save is a slower next start, not a
                // reason to stop.
                app.shell
                    .set_error(format!("could not save the cache: {error:#}"));
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

/// What the loop does after an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Continue,
    /// Something else drew on the terminal; the next frame is a full one.
    Repaint,
    Quit,
}

/// Does what a screen asked for.
fn act(app: &mut App, worker: &Handle, action: AppAction) -> Outcome {
    match action {
        AppAction::Quit => return Outcome::Quit,
        AppAction::Send(request) => {
            if let Err(error) = worker.send(request) {
                app.shell.set_error(format!("{error:#}"));
            }
        }
        AppAction::Exec {
            context,
            namespace,
            pod,
            container,
        } => {
            // bash when the image has it, sh when it does not, in this
            // terminal, with the TUI out of the way until the shell exits.
            let mut command = Command::new("kubectl");
            command.args(["--context", &context, "exec", "-it", "-n", &namespace, &pod]);
            if let Some(container) = &container {
                command.args(["-c", container]);
            }
            command.args([
                "--",
                "sh",
                "-c",
                "command -v bash >/dev/null 2>&1 && exec bash || exec sh",
            ]);
            let status = released_terminal(|| {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .status()
            });
            match status {
                Ok(status) if status.success() => {}
                Ok(status) => app
                    .shell
                    .set_error(format!("kubectl exec on {pod} exited with {status}")),
                Err(error) => app
                    .shell
                    .set_error(format!("kubectl could not be run: {error}")),
            }
            return Outcome::Repaint;
        }
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
    Outcome::Continue
}

/// Runs `body` with the terminal handed back to the shell, and takes it back
/// however `body` went. The caller repaints.
fn released_terminal<T>(body: impl FnOnce() -> T) -> T {
    release_terminal();
    let outcome = body();
    if let Err(error) = claim_terminal() {
        // Nothing can be reported through a TUI that is not there, so this
        // goes where the shell's own output went.
        eprintln!("aks-tui could not take the terminal back: {error:#}");
    }
    outcome
}

/// Puts the terminal back the way the TUI found it: the input features, then
/// raw mode and the alternate screen. The end of a run and the shell hand-off
/// both leave this way.
fn release_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
}

/// Takes the terminal back after [`release_terminal`] gave it away, in the
/// same order `ratatui::init` and the TUI's own startup take it.
fn claim_terminal() -> Result<()> {
    enable_raw_mode().context("failed to take raw mode back")?;
    execute!(io::stdout(), EnterAlternateScreen).context("failed to take the screen back")?;
    enable_terminal_input()
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        // Best effort: the run is over either way, and a terminal that
        // refuses one of these is not something the exit can fix.
        release_terminal();
    }
}

/// The input the TUI reads beyond the keyboard. Turned on here so no later
/// step has to remember to.
fn enable_terminal_input() -> Result<()> {
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)
        .context("failed to enable terminal input features")
}
