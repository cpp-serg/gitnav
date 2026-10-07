mod ansi;
mod app;
mod config;
mod graph;
mod matcher;
mod ops;
mod order;
mod repo;
mod ui;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use app::{App, Command, Dialog};
use ops::PullOutcome;
use config::Settings;

fn main() -> Result<()> {
    let settings = Settings::load()?;
    let history = repo::load(&settings.path, &settings.view)?;
    let mut app = App::new(history, settings.view.clone());
    app.watch = settings.watch;
    app.interval = settings.interval;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &settings.path);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, path: &Path) -> Result<()> {
    let mut next_check = Instant::now() + app.interval;
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;

        let timeout = if app.watch {
            next_check.saturating_duration_since(Instant::now())
        } else {
            Duration::from_secs(3600)
        };
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let was_watching = app.watch;
            app.handle_key(key);
            if app.watch && !was_watching {
                // Catch up on changes made while not watching.
                next_check = Instant::now();
            }
        }

        if let Some(command) = app.command.take() {
            run_command(terminal, app, path, command)?;
            next_check = Instant::now() + app.interval;
        }
        if app.reload {
            app.reload = false;
            // Keep a note the key left, such as which refs it hid.
            let note = app.message.take().unwrap_or_else(|| "reloaded".into());
            reload(app, path, &note);
        } else if app.watch && Instant::now() >= next_check {
            match repo::current_fingerprint(path) {
                Ok(f) if f != app.history.fingerprint => reload(app, path, "repository changed"),
                Ok(_) => {}
                Err(e) => app.message = Some(format!("watch: {e:#}")),
            }
        }
        if app.watch && Instant::now() >= next_check {
            next_check = Instant::now() + app.interval;
        }
    }
    Ok(())
}

fn run_command(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    path: &Path,
    command: Command,
) -> Result<()> {
    match command {
        Command::Pull if ops::rebase_in_progress(path) => {
            app.dialog = Some(rebase_dialog(
                "A rebase is in progress",
                "Finish or abort it before pulling again.",
                ops::conflicted_files(path),
            ));
        }
        Command::Pull => {
            let outcome = outside_tui(terminal, || {
                println!("gitnav: git pull --rebase --autostash");
                let outcome = ops::pull(path);
                if !matches!(outcome, Ok(PullOutcome::Pulled(_))) {
                    // Leave git's messages on the screen until they have been read.
                    print!("\ngitnav: press Enter to go back");
                    let _ = std::io::stdout().flush();
                    let _ = std::io::stdin().read_line(&mut String::new());
                }
                outcome
            })?;
            match outcome {
                Ok(PullOutcome::Pulled(summary)) => app.message = Some(summary),
                Ok(PullOutcome::Failed) => app.message = Some("pull failed".into()),
                Ok(PullOutcome::Conflict(files)) => {
                    app.message = Some("pull stopped on a conflict".into());
                    app.dialog = Some(rebase_dialog(
                        "Pull stopped on a conflict",
                        "The rebase is waiting. Your stashed changes come back when it ends.",
                        files,
                    ))
                }
                Ok(PullOutcome::StashConflict(files)) => {
                    app.message = Some("pulled; local changes conflict".into());
                    let mut lines = vec![
                        "The pull went through, but your local changes conflict with it.".into(),
                        "They are applied with conflict markers and also kept in the stash.".into(),
                        String::new(),
                    ];
                    lines.extend(files.iter().map(|f| format!("  {f}")));
                    app.dialog = Some(Dialog {
                        title: "Local changes conflict".into(),
                        lines,
                        choices: vec![('o', "ok", None)],
                    });
                }
                Err(e) => app.message = Some(format!("pull: {e:#}")),
            }
            reload(app, path, &app.message.clone().unwrap_or_default());
        }
        Command::Show(oid) => match ops::show(path, oid, app.width) {
            Ok(text) => {
                let title = format!("git show {}", &oid.to_string()[..7]);
                app.pager = Some(app::Pager::new(title, ansi::to_lines(&text)));
            }
            Err(e) => app.message = Some(format!("{e:#}")),
        },
        Command::AbortRebase => {
            app.message = Some(match ops::abort_rebase(path) {
                Ok(()) => "rebase aborted".into(),
                Err(e) => format!("{e:#}"),
            });
            reload(app, path, &app.message.clone().unwrap_or_default());
        }
    }
    Ok(())
}

fn rebase_dialog(title: &str, note: &str, files: Vec<String>) -> Dialog {
    let mut lines = vec![note.to_string()];
    if !files.is_empty() {
        lines.push(String::new());
        lines.push("Conflicts:".into());
        lines.extend(files.iter().map(|f| format!("  {f}")));
    }
    Dialog {
        title: title.into(),
        lines,
        choices: vec![
            ('a', "abort the rebase: back to before the pull", Some(Command::AbortRebase)),
            ('l', "leave it as it is, to resolve by hand", None),
        ],
    }
}

/// Runs `f` on the normal screen with the terminal in its usual mode, for programs that talk
/// to the user directly, then brings the TUI back.
fn outside_tui<T>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> T) -> Result<T> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    let result = f();
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    terminal.clear()?;
    Ok(result)
}

/// Loads the history again; on failure the old one stays and the status line says why.
fn reload(app: &mut App, path: &Path, note: &str) {
    match repo::load(path, &app.view) {
        Ok(history) => {
            app.replace(history);
            app.message = Some(note.to_string());
        }
        Err(e) => app.message = Some(format!("reload failed: {e:#}")),
    }
}
