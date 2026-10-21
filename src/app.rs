//! Navigation state and vi-style key handling.

use std::collections::BTreeSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::graph;
use crate::matcher::{self, Mode};
use crate::repo::{self, History, View};

/// Columns moved by one `h` or `l`.
const HSCROLL_STEP: usize = 8;

/// The keys, as the help popup lists them. An entry with no key starts a section.
pub const HELP: &[(&str, &str)] = &[
    ("", "Moving"),
    ("j k  ↓ ↑", "down, up (a count repeats: 5j)"),
    ("gg  G", "first row, last row"),
    ("{n}G  {n}gg", "row n"),
    ("gh", "the layout head (HEAD, or the commit chosen with @), centred"),
    ("Home  End", "first row, last row"),
    ("Ctrl-d  Ctrl-u", "half a page down, up"),
    ("Ctrl-f  Ctrl-b", "a page down, up (also PgDn, PgUp)"),
    ("Ctrl-e  Ctrl-y", "scroll the view a line down, up"),
    ("H  M  L", "top, middle, bottom of the screen"),
    ("zz  zt  zb", "put the cursor row at the centre, top, bottom"),
    ("h l  ← →", "scroll sideways"),
    ("0", "scroll back to the left edge"),
    ("Enter", "show the commit (git show); j k, Ctrl-d Ctrl-u, Space, gg G scroll; q Esc Enter close"),
    ("", "Selecting"),
    ("Space", "mark or unmark the commit, move down"),
    ("v", "start a range; v again marks it"),
    ("Esc", "cancel the range, or clear the search highlight"),
    ("U", "clear all marks"),
    ("", "Search and filter"),
    ("/", "search as you type; Enter keeps the place, Esc goes back"),
    ("n  N", "next, previous match"),
    ("f", "show only matching commits, joined to their nearest matching ancestors"),
    ("F", "clear the filter"),
    ("a", "only the head and what it reaches: on, off"),
    ("Ctrl-f", "in / and f: fuzzy or exact match"),
    ("Ctrl-t", "in / and f: ignore case or match it"),
    ("Up  Ctrl-u  Ctrl-w", "in / and f: previous query, clear, delete a word"),
    ("", "View"),
    ("@", "lay out around the commit as if it were HEAD; again: back to HEAD"),
    ("o", "switch order: topo, date, git-topo"),
    ("d", "deleted commits: hide, reflog, all"),
    ("x", "hide the refs on the commit (or the marked ones) and what only they reach"),
    ("X", "hidden refs: hide, show again"),
    ("^", "first parents only; merges show as ◆━┄ with the count they brought in: on, off"),
    ("r", "only commits with refs, joined to their nearest such ancestors: on, off"),
    ("w", "watch the repository for changes: on, off"),
    ("p", "pull: git pull --rebase --autostash; on a conflict, abort or leave the rebase"),
    ("R", "reload now"),
    ("?", "this help (j k scroll it; ?, Esc or q close it)"),
    ("q  Ctrl-c", "quit"),
];

/// Work for the event loop: things that change the repository or need the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Pull,
    AbortRebase,
    /// Show a commit's contents (`git show`).
    Show(git2::Oid),
}

/// A scrollable text view over the graph, such as a commit's `git show`.
pub struct Pager {
    pub title: String,
    pub lines: Vec<ratatui::text::Line<'static>>,
    pub scroll: usize,
    pub hscroll: usize,
    /// `g` was pressed: a second `g` goes to the top.
    pending_g: bool,
}

impl Pager {
    pub fn new(title: String, lines: Vec<ratatui::text::Line<'static>>) -> Self {
        Pager { title, lines, scroll: 0, hscroll: 0, pending_g: false }
    }

    /// Keeps the scroll position within the text for a view `height` lines tall.
    pub fn clamp(&mut self, height: usize) {
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(height.max(1)));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Search,
    Filter,
}

/// The query line at the bottom while `/` or `f` is being typed.
pub struct Prompt {
    pub kind: PromptKind,
    pub input: String,
    /// What Esc goes back to: the query that was active, and the cursor and scroll position.
    before: (Option<String>, usize, usize),
}

/// A popup that asks for one of its choices, or just shows something (`Esc` closes it).
pub struct Dialog {
    pub title: String,
    pub lines: Vec<String>,
    /// Key, what it does, and the command it runs (none: just close).
    pub choices: Vec<(char, &'static str, Option<Command>)>,
}

/// The rows drawn straight down the left: the head's first-parent line, and above the head,
/// if commits leading to it are newer than it, the line from the newest of them (the topmost
/// among equals) down to the head, by first parents where they lead there.
fn spine(commits: &[repo::Commit], head: usize) -> Vec<usize> {
    // Which rows lead to the head. Children come before parents, so one pass upwards does.
    let mut reaches = vec![false; head + 1];
    reaches[head] = true;
    for r in (0..head).rev() {
        reaches[r] = commits[r].parents.iter().any(|&p| p <= head && reaches[p]);
    }
    let newest = (0..head).rev().filter(|&r| reaches[r]).max_by_key(|&r| commits[r].time);
    let mut row = match newest {
        Some(r) if commits[r].time > commits[head].time => r,
        _ => head,
    };
    let mut spine = Vec::new();
    while row != head {
        spine.push(row);
        row = *commits[row].parents.iter().find(|&&p| p <= head && reaches[p]).unwrap();
    }
    let mut next = Some(head);
    while let Some(r) = next {
        spine.push(r);
        next = commits[r].parents.first().copied();
    }
    spine
}

pub struct App {
    pub history: History,
    pub graph: Vec<graph::Row>,
    /// The row laid out as the head: `view.head` if it is shown, else HEAD's.
    pub head_row: Option<usize>,
    pub cursor: usize,
    /// First visible row.
    pub offset: usize,
    /// Columns scrolled off to the left.
    pub hscroll: usize,
    /// Visible lines, as of the last draw; a row can take more than one.
    pub height: usize,
    /// The window's width, as of the last draw.
    pub width: usize,
    pub marks: BTreeSet<usize>,
    /// Where visual mode started, while it is active.
    pub visual: Option<usize>,
    pub count: Option<usize>,
    /// First key of a two-key command (`g`, `z`).
    pub prefix: Option<char>,
    /// Row order and which deleted commits to show; changing it needs a reload.
    pub view: View,
    /// Whether the event loop polls the repository for changes and reloads.
    pub watch: bool,
    /// Time between checks while watching.
    pub interval: std::time::Duration,
    /// Set by a key that needs the history loaded again; the event loop does the loading.
    pub reload: bool,
    /// A note for the status line, cleared by the next key.
    pub message: Option<String>,
    pub dialog: Option<Dialog>,
    /// Set by a key or dialog choice; the event loop runs it.
    pub command: Option<Command>,
    pub prompt: Option<Prompt>,
    /// The active search, for `n`/`N` and the highlight.
    pub search: Option<String>,
    /// Rows the search matches.
    pub search_hits: usize,
    /// The active filter; `history` then holds only matching commits, `base` all of them.
    pub filter: Option<String>,
    /// Show only the layout head and what it reaches (`a`).
    pub head_only: bool,
    base: Option<History>,
    pub match_mode: Mode,
    /// The last query confirmed in each prompt, for Up.
    last_query: [String; 2],
    /// Go to the head after the next reload (set by `@`), as at startup.
    pub head_to_top: bool,
    /// Centre the cursor row once the screen height is known.
    center_cursor: bool,
    /// The help popup's scroll position, while it is open.
    pub help: Option<usize>,
    /// A commit's contents shown over the graph (Enter).
    pub pager: Option<Pager>,
    pub quit: bool,
}

impl App {
    pub fn new(history: History, view: View) -> Self {
        let mut app = App {
            history,
            graph: Vec::new(),
            head_row: None,
            cursor: 0,
            offset: 0,
            hscroll: 0,
            height: 1,
            width: 80,
            marks: BTreeSet::new(),
            visual: None,
            count: None,
            prefix: None,
            view,
            watch: false,
            interval: std::time::Duration::from_secs(5),
            reload: false,
            message: None,
            dialog: None,
            command: None,
            head_to_top: false,
            center_cursor: false,
            prompt: None,
            search: None,
            search_hits: 0,
            filter: None,
            head_only: false,
            base: None,
            match_mode: Mode::default(),
            last_query: Default::default(),
            help: None,
            pager: None,
            quit: false,
        };
        app.relayout();
        app.go_to_head();
        app
    }

    /// Puts the cursor on the layout head. With nothing newer above it, the head is the top
    /// row; otherwise it is centred, so the newer commits above it are in view as well.
    fn go_to_head(&mut self) {
        let Some(head) = self.head_row else { return };
        self.cursor = head;
        let commits = &self.history.commits;
        if (0..head).any(|r| commits[r].time > commits[head].time) {
            self.center_cursor = true;
        } else {
            self.offset = head;
        }
    }

    /// Called with the screen height known: centres the cursor if that is pending, then
    /// keeps it in view.
    pub fn settle(&mut self) {
        if std::mem::take(&mut self.center_cursor) {
            self.offset = self.top_for(self.cursor, self.height / 2);
        }
        self.clamp();
    }

    /// Lays out the graph again, with the head's line as the spine on the left.
    pub fn relayout(&mut self) {
        let commits = &self.history.commits;
        let chosen = self.view.head.and_then(|h| commits.iter().position(|c| c.oid == h));
        self.head_row = chosen.or(self.history.head);
        let spine = self.head_row.map_or_else(Vec::new, |h| spine(commits, h));
        // The head and everything it reaches are drawn heavy.
        let mut heavy = vec![false; commits.len()];
        let mut todo: Vec<usize> = self.head_row.into_iter().collect();
        while let Some(r) = todo.pop() {
            if !std::mem::replace(&mut heavy[r], true) {
                todo.extend(&commits[r].parents);
            }
        }
        self.graph = graph::layout(commits.iter().map(|c| c.parents.as_slice()), &spine, &heavy);
    }

    pub fn replace(&mut self, history: History) {
        self.swap_rows(|app| {
            app.base = None;
            app.history = history;
            app.apply_filter();
        });
    }

    /// Runs `change`, which replaces the rows, then lays them out again and puts the cursor,
    /// marks and visual anchor back on the same commits.
    fn swap_rows(&mut self, change: impl FnOnce(&mut App)) {
        let oid = |row: usize| self.history.commits.get(row).map(|c| c.oid);
        let cursor = oid(self.cursor);
        let screen_row = self.cursor.saturating_sub(self.offset);
        let marks: Vec<_> = self.marks.iter().filter_map(|&r| oid(r)).collect();
        let anchor = self.visual.and_then(oid);

        change(self);
        let rows: std::collections::HashMap<git2::Oid, usize> =
            self.history.commits.iter().enumerate().map(|(r, c)| (c.oid, r)).collect();
        self.relayout();
        self.count_hits();
        if let Some(&row) = cursor.and_then(|o| rows.get(&o)) {
            self.cursor = row;
            if self.offset > 0 {
                self.offset = row.saturating_sub(screen_row);
            }
        }
        if std::mem::take(&mut self.head_to_top) {
            self.go_to_head();
        }
        self.marks = marks.iter().filter_map(|o| rows.get(o).copied()).collect();
        if self.visual.is_some() {
            self.visual = Some(anchor.and_then(|o| rows.get(&o).copied()).unwrap_or(self.cursor));
        }
        self.clamp();
    }

    pub fn len(&self) -> usize {
        self.history.commits.len()
    }

    fn last(&self) -> usize {
        self.len().saturating_sub(1)
    }

    /// The screen lines a row takes: its commit's, and the connector lines under it.
    fn lines_of(&self, row: usize) -> usize {
        1 + self.graph.get(row).map_or(0, |r| r.below.len())
    }

    /// How many rows from `top` on have their commit's line on the screen.
    fn shown_from(&self, top: usize) -> usize {
        let (mut used, mut rows) = (0, 0);
        while top + rows < self.len() && used < self.height.max(1) {
            used += self.lines_of(top + rows);
            rows += 1;
        }
        rows
    }

    /// The top row that puts `row`'s commit line as far down as screen line `line` goes.
    fn top_for(&self, row: usize, line: usize) -> usize {
        let (mut top, mut used) = (row, 0);
        while top > 0 && used + self.lines_of(top - 1) <= line {
            top -= 1;
            used += self.lines_of(top);
        }
        top
    }

    /// The last row whose commit's line is on the screen.
    fn bottom(&self) -> usize {
        (self.offset + self.shown_from(self.offset).max(1) - 1).min(self.last())
    }

    /// Whether a row shows as selected: marked, or inside the visual range.
    pub fn is_selected(&self, row: usize) -> bool {
        self.marks.contains(&row)
            || self.visual.is_some_and(|a| (a.min(self.cursor)..=a.max(self.cursor)).contains(&row))
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.message = None;
        if let Some(dialog) = &self.dialog {
            // A dialog takes all keys until a choice or Esc closes it.
            if key.code == KeyCode::Esc {
                self.dialog = None;
            } else if let KeyCode::Char(c) = key.code
                && let Some(&(_, _, command)) = dialog.choices.iter().find(|(k, _, _)| *k == c)
            {
                self.command = command;
                self.dialog = None;
            }
            return;
        }
        if let Some(mut pager) = self.pager.take() {
            if self.pager_key(&mut pager, key) {
                self.pager = Some(pager);
            }
            return;
        }
        if let Some(scroll) = self.help {
            // The popup takes all keys until it is closed.
            self.help = match key.code {
                KeyCode::Char('?' | 'q') | KeyCode::Esc => None,
                KeyCode::Char('j') | KeyCode::Down => Some(scroll + 1),
                KeyCode::Char('k') | KeyCode::Up => Some(scroll.saturating_sub(1)),
                _ => Some(scroll),
            };
            return;
        }
        if let Some(prompt) = self.prompt.take() {
            self.prompt_key(prompt, key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let KeyCode::Char(d @ '0'..='9') = key.code
            && !ctrl
            && self.prefix.is_none()
            && (d != '0' || self.count.is_some())
        {
            let d = d.to_digit(10).unwrap() as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d));
            return;
        }
        let count = self.count.take();
        let n = count.unwrap_or(1);
        let prefix = self.prefix.take();
        let page = self.shown_from(self.offset).max(1);

        match (prefix, key.code, ctrl) {
            (Some('g'), KeyCode::Char('g'), false) => self.cursor = count.map_or(0, |c| c - 1),
            (Some('g'), KeyCode::Char('h'), false) => match self.head_row {
                Some(head) => {
                    self.cursor = head;
                    self.center_cursor = true;
                }
                None => self.message = Some("the head is not shown".into()),
            },
            (Some('z'), KeyCode::Char('z'), false) => {
                self.offset = self.top_for(self.cursor, self.height / 2)
            }
            (Some('z'), KeyCode::Char('t'), false) => self.offset = self.cursor,
            (Some('z'), KeyCode::Char('b'), false) => {
                self.offset = self.top_for(self.cursor, self.height.saturating_sub(1))
            }
            (Some(_), _, _) => {}

            (None, KeyCode::Char(c @ ('g' | 'z')), false) => {
                self.prefix = Some(c);
                self.count = count;
            }
            (None, KeyCode::Char('c'), true) | (None, KeyCode::Char('q'), false) => {
                self.quit = true
            }
            (None, KeyCode::Char('j') | KeyCode::Down, false) => self.cursor += n,
            (None, KeyCode::Char('k') | KeyCode::Up, false) => {
                self.cursor = self.cursor.saturating_sub(n)
            }
            (None, KeyCode::Char('G'), false) => self.cursor = count.map_or(self.last(), |c| c - 1),
            (None, KeyCode::Home, false) => self.cursor = 0,
            (None, KeyCode::End, false) => self.cursor = self.last(),
            (None, KeyCode::Char('d'), true) => self.scroll_both(page / 2, true),
            (None, KeyCode::Char('u'), true) => self.scroll_both(page / 2, false),
            (None, KeyCode::Char('f'), true) | (None, KeyCode::PageDown, false) => {
                for _ in 0..n {
                    self.offset += self.shown_from(self.offset).max(1);
                }
                self.offset = self.offset.min(self.max_offset());
                self.cursor = self.cursor.max(self.offset);
            }
            (None, KeyCode::Char('b'), true) | (None, KeyCode::PageUp, false) => {
                for _ in 0..n {
                    // The row above the view goes to the bottom.
                    self.offset = self.top_for(self.offset.saturating_sub(1), self.height.saturating_sub(1));
                }
                self.cursor = self.cursor.min(self.bottom());
            }
            (None, KeyCode::Char('e'), true) => {
                self.offset = (self.offset + n).min(self.max_offset());
                self.cursor = self.cursor.max(self.offset);
            }
            (None, KeyCode::Char('y'), true) => {
                self.offset = self.offset.saturating_sub(n);
                self.cursor = self.cursor.min(self.bottom());
            }
            (None, KeyCode::Char('H'), false) => self.cursor = self.offset + n - 1,
            (None, KeyCode::Char('L'), false) => {
                self.cursor = self.bottom().saturating_sub(n - 1).max(self.offset);
            }
            (None, KeyCode::Char('M'), false) => {
                self.cursor = self.offset + self.shown_from(self.offset).saturating_sub(1) / 2;
            }
            (None, KeyCode::Char('h') | KeyCode::Left, false) => {
                self.hscroll = self.hscroll.saturating_sub(HSCROLL_STEP * n)
            }
            (None, KeyCode::Char('l') | KeyCode::Right, false) => self.hscroll += HSCROLL_STEP * n,
            (None, KeyCode::Char('0'), false) => self.hscroll = 0,
            (None, KeyCode::Char(' '), false) => {
                for _ in 0..n {
                    if self.cursor >= self.len() {
                        break;
                    }
                    if !self.marks.remove(&self.cursor) {
                        self.marks.insert(self.cursor);
                    }
                    self.cursor += 1;
                }
                self.cursor = self.cursor.min(self.last());
            }
            (None, KeyCode::Char('v'), false) => match self.visual.take() {
                Some(anchor) => {
                    self.marks.extend(anchor.min(self.cursor)..=anchor.max(self.cursor))
                }
                None if self.len() > 0 => self.visual = Some(self.cursor),
                None => {}
            },
            (None, KeyCode::Esc, false) => {
                if self.visual.take().is_none() {
                    self.search = None;
                }
            }
            (None, KeyCode::Char('/'), false) => self.open_prompt(PromptKind::Search, String::new()),
            (None, KeyCode::Char('f'), false) => {
                self.open_prompt(PromptKind::Filter, self.filter.clone().unwrap_or_default())
            }
            (None, KeyCode::Char('F'), false) => {
                if self.filter.take().is_some() {
                    self.swap_rows(App::apply_filter);
                }
            }
            (None, KeyCode::Char('a'), false) => {
                self.head_only = !self.head_only;
                self.swap_rows(App::apply_filter);
                if self.head_only && self.head_row.is_some() {
                    self.message = Some(format!("{} commits in the head's history", self.len()));
                }
            }
            (None, KeyCode::Char('n'), false) => self.next_match(true),
            (None, KeyCode::Char('N'), false) => self.next_match(false),
            (None, KeyCode::Char('U'), false) => self.marks.clear(),
            (None, KeyCode::Char('R'), false) => self.reload = true,
            (None, KeyCode::Char('@'), false) => self.set_head(),
            (None, KeyCode::Char('p'), false) => self.command = Some(Command::Pull),
            (None, KeyCode::Enter, false) => {
                if let Some(c) = self.history.commits.get(self.cursor) {
                    self.command = Some(Command::Show(c.oid));
                }
            }
            (None, KeyCode::Char('?'), false) => self.help = Some(0),
            (None, KeyCode::Char('w'), false) => self.watch = !self.watch,
            (None, KeyCode::Char('^'), false) => {
                self.view.first_parent = !self.view.first_parent;
                self.reload = true;
            }
            (None, KeyCode::Char('r'), false) => {
                self.view.refs_only = !self.view.refs_only;
                self.reload = true;
            }
            (None, KeyCode::Char('o'), false) => {
                self.view.order = self.view.order.next();
                self.reload = true;
            }
            (None, KeyCode::Char('x'), false) => self.hide_selected_refs(),
            (None, KeyCode::Char('X'), false) => {
                self.view.hiding = !self.view.hiding;
                self.reload = true;
            }
            (None, KeyCode::Char('d'), false) => {
                self.view.deleted = self.view.deleted.next();
                self.reload = true;
            }
            _ => {}
        }
        self.clamp();
    }

    /// A key while the pager is open; false when it closes the pager.
    fn pager_key(&mut self, p: &mut Pager, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.height.max(1);
        let g = std::mem::take(&mut p.pending_g);
        match (key.code, ctrl) {
            (KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter, false) => return false,
            (KeyCode::Char('c'), true) => {
                self.quit = true;
                return false;
            }
            (KeyCode::Char('j') | KeyCode::Down, false) | (KeyCode::Char('e'), true) => p.scroll += 1,
            (KeyCode::Char('k') | KeyCode::Up, false) | (KeyCode::Char('y'), true) => {
                p.scroll = p.scroll.saturating_sub(1)
            }
            (KeyCode::Char('d'), true) => p.scroll += page / 2,
            (KeyCode::Char('u'), true) => p.scroll = p.scroll.saturating_sub(page / 2),
            (KeyCode::Char('f'), true) | (KeyCode::Char(' ') | KeyCode::PageDown, false) => p.scroll += page,
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, false) => p.scroll = p.scroll.saturating_sub(page),
            (KeyCode::Char('g'), false) if g => p.scroll = 0,
            (KeyCode::Char('g'), false) => p.pending_g = true,
            (KeyCode::Home, false) => p.scroll = 0,
            (KeyCode::Char('G') | KeyCode::End, false) => p.scroll = usize::MAX,
            (KeyCode::Char('h') | KeyCode::Left, false) => p.hscroll = p.hscroll.saturating_sub(HSCROLL_STEP),
            (KeyCode::Char('l') | KeyCode::Right, false) => p.hscroll += HSCROLL_STEP,
            (KeyCode::Char('0'), false) => p.hscroll = 0,
            _ => {}
        }
        p.clamp(page);
        true
    }

    fn open_prompt(&mut self, kind: PromptKind, input: String) {
        let query = match kind {
            PromptKind::Search => self.search.clone(),
            PromptKind::Filter => self.filter.clone(),
        };
        self.prompt = Some(Prompt { kind, input, before: (query, self.cursor, self.offset) });
    }

    /// A key while the prompt is open. The prompt has been taken out of `self.prompt`; it goes
    /// back unless the key closes it. Every change applies at once.
    fn prompt_key(&mut self, mut p: Prompt, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let slot = p.kind as usize;
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => {
                let (query, cursor, offset) = p.before;
                match p.kind {
                    PromptKind::Search => {
                        self.search = query;
                        (self.cursor, self.offset) = (cursor, offset);
                        self.count_hits();
                    }
                    PromptKind::Filter => self.set_filter(query),
                }
                return;
            }
            (KeyCode::Enter, _) => {
                if !p.input.is_empty() {
                    self.last_query[slot] = p.input;
                }
                return;
            }
            (KeyCode::Backspace, _) => {
                p.input.pop();
            }
            (KeyCode::Char('u'), true) => p.input.clear(),
            (KeyCode::Char('w'), true) => {
                let kept = p.input.trim_end().rfind(' ').map_or(0, |i| i + 1);
                p.input.truncate(kept);
            }
            (KeyCode::Up, _) => p.input = self.last_query[slot].clone(),
            (KeyCode::Char('f'), true) => self.match_mode.fuzzy = !self.match_mode.fuzzy,
            (KeyCode::Char('t'), true) => self.match_mode.ignore_case = !self.match_mode.ignore_case,
            (KeyCode::Char(c), false) => p.input.push(c),
            _ => {}
        }
        let query = Some(p.input.clone()).filter(|q| !q.is_empty());
        match p.kind {
            PromptKind::Search => {
                self.search = query;
                self.count_hits();
                // Like vim's incsearch: the first match after where the search started.
                let (_, cursor, offset) = p.before;
                (self.cursor, self.offset) = (cursor, offset);
                if let Some(row) = self.find_from((cursor + 1) % self.len().max(1), true) {
                    self.cursor = row;
                }
                self.clamp();
            }
            PromptKind::Filter => self.set_filter(query),
        }
        self.prompt = Some(p);
    }

    fn matches(&self, row: usize, query: &str) -> bool {
        matcher::find(query, &self.history.line(row), self.match_mode).is_some()
    }

    /// The first row matching the search, from `start` (included) forwards or backwards,
    /// wrapping around.
    fn find_from(&self, start: usize, forward: bool) -> Option<usize> {
        let q = self.search.as_deref()?;
        let n = self.len();
        (0..n)
            .map(|i| if forward { (start + i) % n } else { (start + n - i) % n })
            .find(|&r| self.matches(r, q))
    }

    fn next_match(&mut self, forward: bool) {
        if self.search.is_none() {
            self.message = Some("no search: / starts one".into());
            return;
        }
        let n = self.len().max(1);
        let start = if forward { (self.cursor + 1) % n } else { (self.cursor + n - 1) % n };
        match self.find_from(start, forward) {
            Some(row) => self.cursor = row,
            None => self.message = Some("no match".into()),
        }
    }

    fn count_hits(&mut self) {
        self.search_hits = match self.search.as_deref() {
            Some(q) => (0..self.len()).filter(|&r| self.matches(r, q)).count(),
            None => 0,
        };
    }

    fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter;
        self.swap_rows(App::apply_filter);
    }

    /// Rebuilds the shown rows from all of them: the matching ones, joined up, or all.
    fn apply_filter(&mut self) {
        let all = self.base.take().unwrap_or_else(|| std::mem::take(&mut self.history));
        if self.filter.is_none() && !self.head_only {
            self.history = all;
            return;
        }
        let in_history = if self.head_only { self.head_history(&all) } else { vec![true; all.commits.len()] };
        let mode = self.match_mode;
        let matches = |r: usize| match self.filter.as_deref() {
            Some(q) => matcher::find(q, &all.line(r), mode).is_some(),
            None => true,
        };
        self.history = repo::simplify(&all, |r| in_history[r] && matches(r));
        self.base = Some(all);
    }

    /// Rows of `history` the layout head reaches (itself included); all of them when the head
    /// is not among them.
    fn head_history(&self, history: &History) -> Vec<bool> {
        let commits = &history.commits;
        let chosen = self.view.head.and_then(|h| commits.iter().position(|c| c.oid == h));
        let Some(head) = chosen.or(history.head) else { return vec![true; commits.len()] };
        let mut keep = vec![false; commits.len()];
        let mut todo = vec![head];
        while let Some(r) = todo.pop() {
            if !std::mem::replace(&mut keep[r], true) {
                todo.extend(&commits[r].parents);
            }
        }
        keep
    }

    /// Commits in the repository view, filter or not.
    pub fn total(&self) -> usize {
        self.base.as_ref().map_or(self.len(), |b| b.commits.len())
    }

    /// `@`: lays out around the cursor commit, or back around HEAD when it already is the head.
    /// The order starts with the head, so this reloads.
    fn set_head(&mut self) {
        let Some(c) = self.history.commits.get(self.cursor) else { return };
        if self.view.head == Some(c.oid) {
            self.view.head = None;
            self.message = Some("laid out around HEAD".into());
        } else {
            self.view.head = Some(c.oid);
            self.message = Some(format!("laid out around {}", c.short_id()));
        }
        self.reload = true;
        self.head_to_top = true;
    }

    /// The rows an action applies to: the visual range, else the marks, else the cursor row.
    fn selection(&self) -> Vec<usize> {
        match self.visual {
            Some(a) => (a.min(self.cursor)..=a.max(self.cursor)).collect(),
            None if !self.marks.is_empty() => self.marks.iter().copied().collect(),
            None => vec![self.cursor],
        }
    }

    /// `x`: adds the refs on the selected commits to the hidden ones (a detached HEAD stays),
    /// and switches hiding back on if it was off.
    fn hide_selected_refs(&mut self) {
        let mut refs: Vec<String> = Vec::new();
        for d in self.selection().iter().filter_map(|r| self.history.decorations.get(r)).flatten() {
            if let Some(full) = &d.full
                && !refs.contains(full)
                && !(self.view.hiding && self.view.hide.contains(full))
            {
                refs.push(full.clone());
            }
        }
        if refs.is_empty() {
            self.message = Some("no refs to hide here".into());
            return;
        }
        let names: Vec<&str> = refs.iter().map(|r| crate::repo::short_ref(r)).collect();
        self.message = Some(format!("hid {}", names.join(", ")));
        for r in refs {
            if !self.view.hide.contains(&r) {
                self.view.hide.push(r);
            }
        }
        self.view.hiding = true;
        self.visual = None;
        self.reload = true;
    }

    /// Ctrl-d/Ctrl-u: move the view and the cursor together.
    fn scroll_both(&mut self, by: usize, down: bool) {
        let by = by.max(1);
        if down {
            self.offset = (self.offset + by).min(self.max_offset());
            self.cursor += by;
        } else {
            self.offset = self.offset.saturating_sub(by);
            self.cursor = self.cursor.saturating_sub(by);
        }
    }

    /// The top row that has the last row at the bottom of the screen.
    fn max_offset(&self) -> usize {
        self.top_for(self.last(), self.height.saturating_sub(1))
    }

    /// Keeps the cursor on a row and the view on the cursor.
    pub fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.last());
        self.offset = self.offset.min(self.max_offset());
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + self.shown_from(self.offset) {
            self.offset = self.top_for(self.cursor, self.height.saturating_sub(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{Commit, History, View};
    use ratatui::crossterm::event::KeyEvent;

    fn history(ids: &[u8]) -> History {
        let commits = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| Commit {
                oid: git2::Oid::from_bytes(&[id; 20]).unwrap(),
                parents: if i + 1 < ids.len() { vec![i + 1] } else { vec![] },
                author: String::new(),
                date: String::new(),
                summary: String::new(),
                time: 0,
                merge: false,
                merged: 0,
                deleted: false,
            })
            .collect();
        History { commits, decorations: Default::default(), fingerprint: vec![], rebasing: false, head: Some(0) }
    }

    fn app(rows: usize, height: usize) -> App {
        let ids: Vec<u8> = (0..rows as u8).collect();
        let mut app = App::new(history(&ids), View::default());
        app.height = height;
        app
    }

    fn keys(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(c)));
        }
    }

    fn ctrl(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    #[test]
    fn counts_and_jumps() {
        let mut a = app(100, 10);
        keys(&mut a, "5j");
        assert_eq!(a.cursor, 5);
        keys(&mut a, "G");
        assert_eq!((a.cursor, a.offset), (99, 90));
        keys(&mut a, "gg");
        assert_eq!((a.cursor, a.offset), (0, 0));
        keys(&mut a, "42G");
        assert_eq!(a.cursor, 41);
        keys(&mut a, "7gg");
        assert_eq!(a.cursor, 6);
        keys(&mut a, "k");
        assert_eq!(a.cursor, 5);
        keys(&mut a, "1000j");
        assert_eq!(a.cursor, 99);
    }

    #[test]
    fn screen_relative() {
        let mut a = app(100, 10);
        ctrl(&mut a, 'd');
        assert_eq!((a.cursor, a.offset), (5, 5));
        keys(&mut a, "L");
        assert_eq!(a.cursor, 14);
        keys(&mut a, "M");
        assert_eq!(a.cursor, 9);
        keys(&mut a, "H");
        assert_eq!(a.cursor, 5);
        ctrl(&mut a, 'e');
        assert_eq!((a.cursor, a.offset), (6, 6));
        keys(&mut a, "zz");
        assert_eq!(a.offset, 1);
        keys(&mut a, "zt");
        assert_eq!(a.offset, 6);
        ctrl(&mut a, 'f');
        assert_eq!((a.cursor, a.offset), (16, 16));
        ctrl(&mut a, 'b');
        assert_eq!((a.cursor, a.offset), (15, 6));
    }

    #[test]
    fn marks_and_visual() {
        let mut a = app(20, 10);
        keys(&mut a, " ");
        assert_eq!(a.cursor, 1);
        keys(&mut a, "2 ");
        assert_eq!(a.marks.iter().copied().collect::<Vec<_>>(), vec![0, 1, 2]);
        keys(&mut a, "kk ");
        assert_eq!(a.marks.iter().copied().collect::<Vec<_>>(), vec![0, 2]);
        keys(&mut a, "10Gv3k");
        assert!(a.is_selected(7) && a.is_selected(9) && !a.is_selected(10));
        keys(&mut a, "v");
        assert_eq!(a.visual, None);
        assert_eq!(a.marks.len(), 6);
        keys(&mut a, "U");
        assert!(a.marks.is_empty());
    }

    #[test]
    fn replace_follows_commits() {
        let mut a = app(20, 10);
        keys(&mut a, "3G 3j ");
        let before = (a.cursor, a.offset, a.marks.clone());
        assert_eq!(before.2.iter().copied().collect::<Vec<_>>(), vec![2, 6]);

        // A new commit on top pushes everything down one row.
        let mut ids = vec![100u8];
        ids.extend(0..20u8);
        a.replace(history(&ids));
        assert_eq!(a.cursor, before.0 + 1);
        assert_eq!(a.offset, before.1, "the view stays at the top");
        assert_eq!(a.marks.iter().copied().collect::<Vec<_>>(), vec![3, 7]);

        // The marked commit 2 is gone; the other mark moves up with the rows.
        let ids: Vec<u8> = (0..20u8).filter(|&i| i != 2).collect();
        a.replace(history(&ids));
        assert_eq!(a.marks.iter().copied().collect::<Vec<_>>(), vec![5]);
        assert_eq!(a.cursor, 6);

        // Scrolled down, the cursor keeps its screen row instead.
        keys(&mut a, "15G");
        let (cursor, offset) = (a.cursor, a.offset);
        assert!(offset > 0);
        let mut ids = vec![100u8, 101];
        ids.extend((0..20u8).filter(|&i| i != 2));
        a.replace(history(&ids));
        assert_eq!((a.cursor, a.offset), (cursor + 2, offset + 2));
    }

    #[test]
    fn help_takes_keys_until_closed() {
        let mut a = app(20, 10);
        keys(&mut a, "?jjk");
        assert_eq!((a.help, a.cursor), (Some(1), 0));
        keys(&mut a, "q");
        assert_eq!((a.help, a.quit), (None, false));
        keys(&mut a, "j?");
        a.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!((a.help, a.cursor), (None, 1));
    }

    #[test]
    fn x_hides_refs_of_the_selection() {
        use crate::repo::{Decoration, RefKind};
        let mut a = app(10, 10);
        let dec = |name: &str, full: Option<&str>| Decoration {
            kind: RefKind::Local,
            name: name.into(),
            full: full.map(Into::into),
        };
        a.history.decorations.insert(0, vec![dec("HEAD", None)]);
        a.history.decorations.insert(2, vec![dec("HEAD -> main", Some("refs/heads/main"))]);
        a.history.decorations.insert(4, vec![dec("v1", Some("refs/tags/v1"))]);

        keys(&mut a, "x");
        assert!(a.view.hide.is_empty() && !a.reload, "a detached HEAD is not hidden");
        keys(&mut a, "3G j x");
        assert_eq!(a.view.hide, ["refs/heads/main", "refs/tags/v1"]);
        assert_eq!(a.message.as_deref(), Some("hid main, v1"));
        assert!(a.reload);

        a.reload = false;
        keys(&mut a, "X");
        assert!(!a.view.hiding && a.reload);
        keys(&mut a, "U5Gx");
        assert!(a.view.hiding, "x switches hiding back on");
        assert_eq!(a.view.hide.len(), 2, "no duplicates");
    }

    #[test]
    fn dialog_takes_keys_until_answered() {
        let mut a = app(10, 10);
        keys(&mut a, "p");
        assert_eq!(a.command.take(), Some(Command::Pull));
        a.dialog = Some(Dialog {
            title: "t".into(),
            lines: vec![],
            choices: vec![('a', "abort", Some(Command::AbortRebase)), ('l', "leave", None)],
        });
        keys(&mut a, "jq");
        assert!(a.dialog.is_some() && !a.quit && a.cursor == 0);
        keys(&mut a, "a");
        assert_eq!((a.dialog.is_none(), a.command), (true, Some(Command::AbortRebase)));
    }

    #[test]
    fn at_sets_and_resets_the_layout_head() {
        let mut a = app(5, 10);
        assert_eq!(a.head_row, Some(0));
        keys(&mut a, "3G@");
        assert!(a.reload);
        // The event loop reloads; here the rows stay as they are.
        a.relayout();
        assert_eq!((a.head_row, a.view.head), (Some(2), Some(a.history.commits[2].oid)));
        // Rows above the head keep off the spine's lane.
        assert_eq!(a.graph[0].cells[0].glyph, ' ');
        assert_eq!(a.graph[2].cells[0].glyph, '●');
        keys(&mut a, "@");
        a.relayout();
        assert_eq!((a.head_row, a.view.head), (Some(0), None));
    }

    #[test]
    fn head_starts_at_the_top() {
        let ids: Vec<u8> = (0..30).collect();
        let mut h = history(&ids);
        h.head = Some(7);
        let mut a = App::new(h, View::default());
        a.height = 10;
        a.settle();
        assert_eq!((a.cursor, a.offset), (7, 7));

        keys(&mut a, "12G@");
        a.replace(history(&ids));
        assert_eq!((a.cursor, a.offset), (11, 11), "after @ the new head is the top row");
        keys(&mut a, "k");
        a.replace(history(&ids));
        assert_eq!(a.offset, 10, "only once");
    }

    /// Rows 0..n with the given subjects.
    fn titled(subjects: &[&str]) -> App {
        let ids: Vec<u8> = (0..subjects.len() as u8).collect();
        let mut h = history(&ids);
        for (c, s) in h.commits.iter_mut().zip(subjects) {
            c.summary = s.to_string();
        }
        let mut a = App::new(h, View::default());
        a.height = 10;
        a
    }

    fn special(a: &mut App, code: KeyCode) {
        a.handle_key(KeyEvent::from(code));
    }

    #[test]
    fn search_as_you_type_and_go_back() {
        let subjects = ["one", "Fix parser", "two", "fix lexer", "three"];
        let mut a = titled(&subjects);
        keys(&mut a, "2G/fix");
        assert_eq!(a.cursor, 3, "first match after where the search started");
        assert_eq!(a.search_hits, 2, "case is ignored");
        special(&mut a, KeyCode::Esc);
        assert_eq!((a.cursor, a.search.as_deref()), (1, None), "Esc goes back");

        keys(&mut a, "/fix");
        special(&mut a, KeyCode::Enter);
        assert_eq!(a.cursor, 3);
        keys(&mut a, "n");
        assert_eq!(a.cursor, 1, "wraps around");
        keys(&mut a, "n");
        assert_eq!(a.cursor, 3);
        keys(&mut a, "N");
        assert_eq!(a.cursor, 1);

        // Fuzzy by default; exact and case-sensitive by toggles.
        keys(&mut a, "/fxlx");
        assert_eq!(a.search_hits, 1);
        a.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(a.search_hits, 0);
        a.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        keys(&mut a, "Fix");
        assert_eq!(a.search_hits, 2);
        a.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(a.search_hits, 1);
        special(&mut a, KeyCode::Enter);

        special(&mut a, KeyCode::Esc);
        assert_eq!(a.search, None, "Esc clears the highlight");
        keys(&mut a, "/");
        special(&mut a, KeyCode::Up);
        assert_eq!(a.prompt.as_ref().unwrap().input, "Fix");
    }

    #[test]
    fn filter_keeps_matching_commits_joined() {
        let subjects = ["fix a", "other", "fix b", "more", "base fix"];
        let mut a = titled(&subjects);
        keys(&mut a, "ffix");
        assert_eq!(a.len(), 3);
        assert_eq!(a.total(), 5);
        let rows: Vec<&str> = a.history.commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(rows, ["fix a", "fix b", "base fix"]);
        assert_eq!(a.history.commits[0].parents, [1], "joined past the hidden commits");
        special(&mut a, KeyCode::Enter);

        // The prompt starts with the current filter; Esc puts it back as it was.
        keys(&mut a, "f");
        assert_eq!(a.prompt.as_ref().unwrap().input, "fix");
        keys(&mut a, " b");
        assert_eq!(a.len(), 1);
        special(&mut a, KeyCode::Esc);
        assert_eq!((a.len(), a.filter.as_deref()), (3, Some("fix")));

        keys(&mut a, "F");
        assert_eq!((a.len(), a.filter.as_deref()), (5, None));
    }

    #[test]
    fn start_on_the_head() {
        let ids: Vec<u8> = (0..30).collect();
        let timed = |head: usize, times: &[(usize, i64)]| {
            let mut h = history(&ids);
            h.head = Some(head);
            for c in &mut h.commits {
                c.time = 100;
            }
            for &(r, t) in times {
                h.commits[r].time = t;
            }
            let mut a = App::new(h, View::default());
            a.height = 10;
            a.settle();
            (a.cursor, a.offset)
        };
        // Nothing above the head is newer: the head is the top row.
        assert_eq!(timed(7, &[(2, 50)]), (7, 7));
        // Something above it is newer: the head is centred, with those above it in view. The
        // rows above it join the head's line on a connector line, which takes a screen line.
        assert_eq!(timed(7, &[(2, 150), (5, 200)]), (7, 3));
        assert_eq!(timed(3, &[(0, 150)]), (3, 0));
    }

    #[test]
    fn scrolling_counts_connector_lines() {
        // 0 merges 1 and 2, both on 3: a connector line under 0 (│╲) and under 2 (│╱), so the
        // four rows take six lines.
        let mut h = history(&[0, 1, 2, 3]);
        h.commits[0].parents = vec![1, 2];
        h.commits[1].parents = vec![3];
        let mut a = App::new(h, View::default());
        assert_eq!(a.graph.iter().map(|r| r.below.len()).collect::<Vec<_>>(), [1, 0, 1, 0]);
        a.height = 3;
        keys(&mut a, "G");
        a.clamp();
        assert_eq!((a.cursor, a.offset), (3, 2), "2, its connector line and 3 fill the screen");
        ctrl(&mut a, 'b');
        assert_eq!((a.cursor, a.offset), (1, 0), "0 and its connector line, then 1");
        keys(&mut a, "M");
        assert_eq!(a.cursor, 0);
        ctrl(&mut a, 'f');
        assert_eq!((a.cursor, a.offset), (2, 2));
    }

    #[test]
    fn gh_goes_to_the_head_centred() {
        let ids: Vec<u8> = (0..40).collect();
        let mut h = history(&ids);
        h.head = Some(20);
        let mut a = App::new(h, View::default());
        a.height = 10;
        a.settle();
        keys(&mut a, "G");
        a.settle();
        keys(&mut a, "gh");
        a.settle();
        // Centred: the connector line above the head counts.
        assert_eq!((a.cursor, a.offset), (20, 16));
    }

    #[test]
    fn spine_runs_through_newer_commits_above_the_head() {
        // a-b-c-d-e-f, a newest on top, the head at d: one straight line.
        let ids: Vec<u8> = (0..6).collect();
        let mut h = history(&ids);
        h.head = Some(3);
        for (i, c) in h.commits.iter_mut().enumerate() {
            c.time = 100 - i as i64;
        }
        let a = App::new(h, View::default());
        assert!(a.graph.iter().all(|row| row.cells.len() == 1 && row.cells[0].glyph == '●'), "a straight line");
        assert_eq!(a.head_row, Some(3));
        let heavy: Vec<bool> = a.graph.iter().map(|row| row.cells[0].v_heavy).collect();
        assert_eq!(heavy, [false, false, false, true, true, true], "heavy from the head down");
    }

    #[test]
    fn spine_takes_the_newest_line_to_the_head() {
        // Rows: 0 (newest) on 2; 1 on 2 as well; 2 is the head, on 3.
        let ids: Vec<u8> = (0..4).collect();
        let mut h = history(&ids);
        h.commits[0].parents = vec![2];
        h.commits[1].parents = vec![2];
        h.head = Some(2);
        for (c, t) in h.commits.iter_mut().zip([50, 40, 30, 20]) {
            c.time = t;
        }
        assert_eq!(spine(&h.commits, 2), [0, 2, 3]);
        // If nothing above is newer, the spine starts at the head.
        for c in &mut h.commits {
            c.time = 0;
        }
        assert_eq!(spine(&h.commits, 2), [2, 3]);
    }

    #[test]
    fn a_shows_only_the_head_history() {
        // Rows: 0 on 2 (a newer tip), 1 on 2 (the head), 2 on 4, 3 on 4 (another branch),
        // 4 the root. The head reaches 1, 2 and 4.
        let ids: Vec<u8> = (0..5).collect();
        let mut h = history(&ids);
        h.commits[0].parents = vec![2];
        h.commits[1].parents = vec![2];
        h.commits[2].parents = vec![4];
        h.commits[3].parents = vec![4];
        h.head = Some(1);
        let mut a = App::new(h, View::default());
        a.height = 10;
        keys(&mut a, "a");
        let ids: Vec<u8> = a.history.commits.iter().map(|c| c.oid.as_bytes()[0]).collect();
        assert_eq!(ids, [1, 2, 4]);
        assert_eq!(a.history.commits[0].parents, [1], "edges stay as they were");
        assert_eq!((a.total(), a.head_row), (5, Some(0)));
        keys(&mut a, "a");
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn enter_shows_and_the_pager_scrolls() {
        let mut a = app(20, 10);
        keys(&mut a, "3G");
        special(&mut a, KeyCode::Enter);
        assert_eq!(a.command.take(), Some(Command::Show(a.history.commits[2].oid)));

        let text: Vec<ratatui::text::Line<'static>> = (0..50).map(|i| ratatui::text::Line::raw(format!("line {i}"))).collect();
        a.pager = Some(Pager::new("show".into(), text));
        keys(&mut a, "jjj");
        assert_eq!(a.pager.as_ref().unwrap().scroll, 3);
        assert_eq!(a.cursor, 2, "the graph does not move under the pager");
        keys(&mut a, "G");
        assert_eq!(a.pager.as_ref().unwrap().scroll, 40, "last page");
        keys(&mut a, "gg");
        assert_eq!(a.pager.as_ref().unwrap().scroll, 0);
        keys(&mut a, "q");
        assert!(a.pager.is_none() && !a.quit);
    }
}
