mod ansi;
mod app;
mod config;
mod graph;
mod matcher;
mod ops;
mod order;
mod repo;
mod state;
mod ui;

use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use app::{App, Command, Dialog};
use ops::{Change, DiffTool, PullOutcome};
use config::Settings;

fn main() -> Result<()> {
    let settings = Settings::load()?;
    let history = repo::load(&settings.path, &settings.view)?;
    let mut app = App::new(history, settings.view.clone());
    app.watch = settings.watch;
    app.interval = settings.interval;
    app.diff_tool = state::load().diff_tool;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &settings.path);
    ratatui::restore();
    result
}

/// How often the loop looks for answers while merges are being worked out.
const CLASSIFY_POLL: Duration = Duration::from_millis(30);

/// A thread that works out what merges did (`repo::merge_kind`), one at a time as asked, so
/// that a slow one never holds up the keys. Returns where to ask and where the answers come.
fn merge_worker(path: &Path) -> (mpsc::Sender<git2::Oid>, mpsc::Receiver<(git2::Oid, repo::MergeKind)>) {
    let (ask, asked) = mpsc::channel::<git2::Oid>();
    let (answer, answers) = mpsc::channel();
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let repo = git2::Repository::discover(&path).ok();
        for oid in asked {
            let kind = repo.as_ref().map_or(repo::MergeKind::Unknown, |r| repo::merge_kind(r, oid));
            if answer.send((oid, kind)).is_err() {
                break;
            }
        }
    });
    (ask, answers)
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, path: &Path) -> Result<()> {
    let mut next_check = Instant::now() + app.interval;
    let (ask, answers) = merge_worker(path);
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        // Only the merges on screen, now that the screen's rows are known.
        for oid in app.merges_to_classify() {
            let _ = ask.send(oid);
        }

        let mut timeout = if app.watch {
            next_check.saturating_duration_since(Instant::now())
        } else {
            Duration::from_secs(3600)
        };
        if app.classifying() {
            timeout = timeout.min(CLASSIFY_POLL);
        }
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

        while let Ok((oid, kind)) = answers.try_recv() {
            app.merge_classified(oid, kind);
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
        Command::FindDeletables(oid) => {
            let short = oid.to_string()[..7].to_string();
            // Asking the remotes about tags can take a moment: say so first.
            app.message = Some(format!("looking for branches and tags at {short}…"));
            terminal.draw(|frame| ui::draw(frame, app))?;
            app.message = None;
            match ops::deletables(path, oid) {
                Ok(found) if found.entries.is_empty() && found.notes.is_empty() => {
                    app.message = Some(format!("no branches or tags at {short}"));
                }
                Ok(found) if found.entries.is_empty() => {
                    app.dialog = Some(Dialog {
                        title: format!("Nothing to delete at {short}"),
                        lines: found.notes,
                        choices: vec![('o', "ok", None)],
                    });
                }
                Ok(found) => app.delete = Some(app::DeleteDialog::new(short, found)),
                Err(e) => app.message = Some(format!("{e:#}")),
            }
        }
        Command::Delete => {
            let chosen = std::mem::take(&mut app.deletions);
            let run = |what: &ops::Deletion| ops::delete(path, what);
            let results: Vec<_> = if chosen.iter().any(ops::Deletion::is_remote) {
                // Deleting on a remote is a push, which may ask for a passphrase.
                outside_tui(terminal, || {
                    let results: Vec<_> = chosen
                        .iter()
                        .map(|what| {
                            let (kind, name) = what.label();
                            println!("gitnav: deleting {kind} {name}");
                            let result = run(what);
                            match &result {
                                Ok(said) => println!("  {said}"),
                                Err(e) => println!("  {e:#}"),
                            }
                            result
                        })
                        .collect();
                    if results.iter().any(Result::is_err) {
                        print!("\ngitnav: press Enter to go back");
                        let _ = std::io::stdout().flush();
                        let _ = std::io::stdin().read_line(&mut String::new());
                    }
                    results
                })?
            } else {
                chosen.iter().map(run).collect()
            };
            let failed: Vec<String> = results.iter().filter_map(|r| r.as_ref().err().map(|e| format!("{e:#}"))).collect();
            let done = results.len() - failed.len();
            let note = match failed.first() {
                None => format!("deleted {done} of {}", results.len()),
                Some(first) => format!("deleted {done} of {}; {first}", results.len()),
            };
            reload(app, path, &note);
        }
        Command::Show(oid) => show(terminal, app, path, Change::Commit(oid))?,
        Command::Resolution(oid) => {
            // diffnav takes plain diffs; the resolution reads better with git's own notes.
            let tool = if app.diff_tool == DiffTool::Difft && app.diff_tool.available() { DiffTool::Difft } else { DiffTool::Git };
            match ops::resolution(path, oid, tool, app.width) {
                Ok(Some(text)) => {
                    let title = format!("remerge-diff {}", repo::short(oid));
                    app.pager = Some(app::Pager::new(title, ansi::to_lines(&text)));
                }
                Ok(None) => app.message = Some(format!("{}: nothing beyond the automatic merge", repo::short(oid))),
                Err(e) => app.message = Some(format!("{e:#}")),
            }
        }
        Command::Diff(first, second) => match ops::diff_order(path, first, second) {
            Ok((from, to)) => show(terminal, app, path, Change::Between(from, to))?,
            Err(e) => app.message = Some(format!("{e:#}")),
        },
        Command::SaveState => {
            let tool = app.diff_tool;
            let mut note = format!("diff tool: {}", tool.name());
            if !tool.available() {
                note += &format!(" (not installed: {} instead)", DiffTool::Git.name());
            }
            if let Err(e) = state::save(&state::State { diff_tool: tool }) {
                note += &format!("; {e:#}");
            }
            app.message = Some(note);
        }
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

/// Enter and `d`: `change` in the chosen tool, or in git if that is missing.
fn show(terminal: &mut DefaultTerminal, app: &mut App, path: &Path, change: Change) -> Result<()> {
    let change = match change.resolve(path) {
        Ok(change) => change,
        Err(e) => {
            app.message = Some(format!("{e:#}"));
            return Ok(());
        }
    };
    let chosen = app.diff_tool;
    let tool = if chosen.available() { chosen } else { DiffTool::Git };
    // diffnav shows only the diff, and git a commit's message too, so an empty one is worth it.
    match ops::changes_nothing(path, change) {
        Ok(true) if tool == DiffTool::Diffnav || matches!(change, Change::Between(..)) => {
            app.message = Some("no differences".into());
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => {
            app.message = Some(format!("{e:#}"));
            return Ok(());
        }
    }
    if tool == DiffTool::Diffnav {
        if let Err(e) = outside_tui(terminal, || ops::diffnav(path, change))? {
            app.message = Some(format!("{e:#}"));
        } else if let Change::Commit(c) = change
            && app.merge_kinds.get(&c) == Some(&repo::MergeKind::Resolved)
        {
            // diffnav showed the merge against its first parent only.
            app.message = Some(format!("{} has changes made by hand in the merge: m shows them", repo::short(c)));
        }
        return Ok(());
    }
    match ops::text(path, change, tool, app.width) {
        Ok(text) => {
            app.pager = Some(app::Pager::new(change.title(tool), ansi::to_lines(&text)));
            if tool != chosen {
                app.message = Some(format!("{} is not installed: {} instead", chosen.name(), tool.name()));
            }
        }
        Err(e) => app.message = Some(format!("{e:#}")),
    }
    Ok(())
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
