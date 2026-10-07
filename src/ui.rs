//! Drawing: the commit list (graph plus `logpretty` fields) and a status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Dialog, HELP, Prompt, PromptKind};
use crate::matcher;

/// Lane colours, in the order git cycles through them.
const LANE_COLORS: [Color; crate::graph::COLORS as usize] = [
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
];

const CURSOR_BG: Color = Color::Indexed(238);
const SELECTED_BG: Color = Color::Indexed(17);
const CURSOR_SELECTED_BG: Color = Color::Indexed(24);
const MARK_GUTTER: Color = Color::Yellow;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [list, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [gutter, list] = Layout::horizontal([Constraint::Length(1), Constraint::Min(1)]).areas(list);

    app.height = list.height as usize;
    app.width = frame.area().width as usize;
    app.settle();

    if let Some(pager) = &mut app.pager {
        // The commit's contents take the whole window above the status line.
        let area = Rect { x: gutter.x, width: gutter.width + list.width, ..list };
        pager.clamp(area.height as usize);
        let shown = Paragraph::new(pager.lines.clone()).scroll((pager.scroll as u16, pager.hscroll as u16));
        frame.render_widget(shown, area);
        let last = (pager.scroll + area.height as usize).min(pager.lines.len());
        let info = format!(" {}  lines {}-{} of {}", pager.title, pager.scroll + 1, last, pager.lines.len());
        let hint = "q close ";
        let pad = (status.width as usize).saturating_sub(info.chars().count() + hint.len());
        let style = Style::new().fg(Color::Black).bg(Color::Gray);
        frame.render_widget(Line::styled(format!("{info}{}{hint}", " ".repeat(pad)), style), status);
        return;
    }

    let rows = app.offset..(app.offset + app.height).min(app.len());
    let lines: Vec<Line> = rows.clone().map(|row| commit_line(app, row)).collect();
    frame.render_widget(Paragraph::new(lines).scroll((0, app.hscroll as u16)), list);

    let buf = frame.buffer_mut();
    for (y, row) in (list.y..).zip(rows) {
        let selected = app.is_selected(row);
        let bg = match (row == app.cursor, selected) {
            (true, true) => Some(CURSOR_SELECTED_BG),
            (true, false) => Some(CURSOR_BG),
            (false, true) => Some(SELECTED_BG),
            (false, false) => None,
        };
        if let Some(bg) = bg {
            // Only where nothing set a background, so match highlights stay visible.
            for x in list.left()..list.right() {
                let cell = &mut buf[(x, y)];
                if cell.bg == Color::Reset {
                    cell.set_bg(bg);
                }
            }
        }
        if selected {
            buf[(gutter.x, y)].set_char('▍').set_fg(MARK_GUTTER);
        } else if app.head_row == Some(row) {
            buf[(gutter.x, y)].set_char('▶').set_fg(head_color(app, row));
        }
    }

    match &app.prompt {
        Some(prompt) => {
            let (line, cursor_x) = prompt_line(app, prompt, status.width as usize);
            frame.render_widget(line, status);
            frame.set_cursor_position((status.x + cursor_x, status.y));
        }
        None => frame.render_widget(status_line(app, status.width as usize), status),
    }
    if app.help.is_some() {
        draw_help(frame, app);
    }
    if let Some(dialog) = &app.dialog {
        draw_dialog(frame, dialog);
    }
}

/// The key list in a bordered popup over the commits, scrolled when it does not fit.
fn draw_help(frame: &mut Frame, app: &mut App) {
    let key_width = HELP.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|&(key, text)| {
            if key.is_empty() {
                Line::styled(text, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {key:<key_width$}  "), Style::new().fg(Color::Cyan)),
                    Span::raw(text),
                ])
            }
        })
        .collect();
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let area = frame.area();
    let popup = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(lines.len() as u16 + 2) / 2,
        width: width.min(area.width),
        height: (lines.len() as u16 + 2).min(area.height),
    };
    let max_scroll = lines.len().saturating_sub(popup.height.saturating_sub(2) as usize);
    let scroll = app.help.unwrap_or(0).min(max_scroll);
    app.help = Some(scroll);

    let block = Block::bordered()
        .title(" keys ")
        .title_bottom(Line::from(" ? Esc q close ").right_aligned())
        .border_style(Style::new().fg(Color::Gray));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll as u16, 0)),
        popup,
    );
}

fn draw_dialog(frame: &mut Frame, dialog: &Dialog) {
    let mut lines: Vec<Line> = dialog.lines.iter().map(|l| Line::raw(l.as_str())).collect();
    lines.push(Line::raw(""));
    for &(key, text, _) in &dialog.choices {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key}  "), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(text),
        ]));
    }
    let width = lines
        .iter()
        .map(Line::width)
        .chain([dialog.title.chars().count() + 2])
        .max()
        .unwrap_or(0) as u16
        + 4;
    let area = frame.area();
    let height = lines.len() as u16 + 2;
    let popup = Rect {
        x: area.width.saturating_sub(width) / 2,
        y: area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    let block = Block::bordered()
        .title(format!(" {} ", dialog.title))
        .title_bottom(Line::from(" Esc close ").right_aligned())
        .border_style(Style::new().fg(Color::Yellow));
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The layout head's colour: green for HEAD, magenta for a commit chosen with `@`.
fn head_color(app: &App, row: usize) -> Color {
    let chosen = app.view.head.is_some_and(|h| app.history.commits[row].oid == h);
    if chosen { Color::LightMagenta } else { Color::LightGreen }
}

fn lane(i: u8) -> Color {
    LANE_COLORS[i as usize % LANE_COLORS.len()]
}

/// One row, formatted like `logpretty`: `%h - (%as) %s -%d(%an)` after the graph.
fn commit_line(app: &App, row: usize) -> Line<'_> {
    let c = &app.history.commits[row];
    let mut spans: Vec<Span> = Vec::new();
    let is_head = app.head_row == Some(row);
    for cell in app.graph.get(row).map_or(&[][..], |r| r) {
        let (glyph, conn) = cell.shown();
        let weight = |heavy: bool| if heavy { Modifier::BOLD } else { Modifier::empty() };
        let style = Style::new().fg(lane(cell.color)).add_modifier(weight(cell.v_heavy || cell.h_heavy));
        // With first parents only, a merge's other side is not drawn: mark the merge.
        let hidden_merge = glyph == '●' && c.merge && app.view.first_parent;
        let (glyph, style) = match glyph {
            '●' if is_head => ('◉', Style::new().fg(head_color(app, row)).add_modifier(Modifier::BOLD)),
            '●' if hidden_merge => ('◆', style.add_modifier(Modifier::BOLD)),
            '●' if c.deleted => ('○', style),
            g => (g, style),
        };
        spans.push(Span::styled(glyph.to_string(), style));
        let conn_style = Style::new().fg(lane(cell.conn_color)).add_modifier(weight(cell.conn_heavy));
        if hidden_merge && conn == ' ' {
            // A stub for the merged side that is not shown. With a line to the right only the
            // one-character gap is free for it.
            let last = app.graph.get(row).is_some_and(|r| std::ptr::eq(cell, r.last().unwrap()));
            spans.push(Span::styled(if last { "━┄ " } else { "━" }, style));
        } else {
            spans.push(Span::styled(conn.to_string(), conn_style));
        }
    }

    let text_start = spans.len();
    let bold = Modifier::BOLD;
    let id_style = if is_head {
        // The head's hash as a badge, so the row stands out.
        Style::new().fg(Color::Black).bg(head_color(app, row)).add_modifier(bold)
    } else {
        Style::new().fg(Color::Blue).add_modifier(bold)
    };
    spans.push(Span::styled(c.short_id(), id_style));
    // A merge whose other side is hidden: a long dash too, besides its ◆.
    let sep = if c.merge && app.view.first_parent { " — " } else { " - " };
    spans.push(Span::raw(sep));
    spans.push(Span::styled(format!("({})", c.date), Style::new().fg(Color::Green).add_modifier(bold)));
    spans.push(Span::raw(" "));
    let summary_style = Style::new().fg(Color::White).add_modifier(bold);
    // A merge's leading "Merge" or "Merged", set apart from the rest of the subject.
    let word = ["Merged", "Merge"].into_iter().find(|w| c.merge && c.summary.starts_with(w));
    if let Some(word) = word {
        spans.push(Span::styled(word, Style::new().fg(Color::LightMagenta).add_modifier(bold)));
        spans.push(Span::styled(&c.summary[word.len()..], summary_style));
    } else {
        spans.push(Span::styled(c.summary.as_str(), summary_style));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled("-", Style::new().fg(Color::White).add_modifier(Modifier::DIM)));
    if let Some(decs) = app.history.decorations.get(&row) {
        let names: Vec<&str> = decs.iter().map(|d| d.name.as_str()).collect();
        spans.push(Span::styled(
            format!(" ({})", names.join(", ")),
            Style::new().fg(Color::LightYellow).add_modifier(bold | Modifier::REVERSED),
        ));
    }
    spans.push(Span::styled(format!("({})", c.author), Style::new().fg(Color::Blue)));
    if c.deleted {
        // Commits no ref reaches: dim the text, keep the graph as it is.
        for span in &mut spans[text_start..] {
            span.style = span.style.add_modifier(Modifier::DIM | Modifier::ITALIC);
        }
    }
    // The search's matches, or else the filter's.
    let query = app.search.as_deref().or(app.filter.as_deref());
    if let Some(positions) = query.and_then(|q| matcher::find(q, &app.history.line(row), app.match_mode)) {
        let text = spans.split_off(text_start);
        spans.extend(highlight(text, &positions));
    }
    // After the hash, how many commits the merge brought in. Added after the highlighting,
    // which counts characters of the row's text.
    if marked_merge(app, row) {
        let count = Span::styled(format!(" +{}", c.merged), Style::new().fg(Color::LightCyan).add_modifier(bold));
        insert_at(&mut spans, text_start, c.short_id().chars().count(), count);
    }
    Line::from(spans)
}

/// Inserts `new` `chars` characters into the spans from `from` on, splitting a span if needed.
fn insert_at<'a>(spans: &mut Vec<Span<'a>>, from: usize, chars: usize, new: Span<'a>) {
    let mut left = chars;
    for i in from..spans.len() {
        let len = spans[i].content.chars().count();
        if left <= len {
            if left < len {
                let split = spans[i].content.char_indices().nth(left).map_or(0, |(b, _)| b);
                let (head, tail) = spans[i].content.split_at(split);
                let (head, tail) = (head.to_string(), tail.to_string());
                let style = spans[i].style;
                spans[i] = Span::styled(head, style);
                spans.insert(i + 1, Span::styled(tail, style));
            }
            spans.insert(i + 1, new);
            return;
        }
        left -= len;
    }
    spans.push(new);
}

/// A merge whose other side is not shown (first parents only).
fn marked_merge(app: &App, row: usize) -> bool {
    app.view.first_parent && app.history.commits[row].merge
}


/// Splits `spans` so that the characters at `positions` (counted across all of them) stand
/// out.
fn highlight<'a>(spans: Vec<Span<'a>>, positions: &[usize]) -> Vec<Span<'a>> {
    let mark = |s: Style| s.bg(Color::Yellow).fg(Color::Black).remove_modifier(Modifier::REVERSED);
    let mut out = Vec::new();
    let mut next = positions.iter().copied().peekable();
    let mut i = 0;
    for span in spans {
        let mut run = String::new();
        let mut run_marked = false;
        for ch in span.content.chars() {
            let marked = next.peek() == Some(&i);
            if marked {
                next.next();
            }
            if marked != run_marked && !run.is_empty() {
                let style = if run_marked { mark(span.style) } else { span.style };
                out.push(Span::styled(std::mem::take(&mut run), style));
            }
            run_marked = marked;
            run.push(ch);
            i += 1;
        }
        if !run.is_empty() {
            out.push(Span::styled(run, if run_marked { mark(span.style) } else { span.style }));
        }
    }
    out
}

/// The query being typed, and where the terminal cursor goes in it.
fn prompt_line(app: &App, prompt: &Prompt, width: usize) -> (Line<'static>, u16) {
    let label = match prompt.kind {
        PromptKind::Search => "/".to_string(),
        PromptKind::Filter => "filter: ".to_string(),
    };
    let found = match prompt.kind {
        PromptKind::Search if app.search.is_some() => format!("{} found", app.search_hits),
        PromptKind::Filter if app.filter.is_some() => format!("{} of {}", app.len(), app.total()),
        _ => String::new(),
    };
    let right = format!("{found}  {}  ^F fuzzy/exact  ^T case ", app.match_mode.describe());
    let left = format!("{label}{}", prompt.input);
    let cursor = left.chars().count() as u16;
    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    let line = Line::from(vec![
        Span::raw(left),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::new().fg(Color::DarkGray)),
    ]);
    (line, cursor)
}

fn status_line(app: &App, width: usize) -> Line<'_> {
    let style = Style::new().fg(Color::Black).bg(Color::Gray);
    let mut left = if app.len() == 0 {
        " no commits".to_string()
    } else {
        let c = &app.history.commits[app.cursor];
        format!(" {}  {}/{}", c.short_id(), app.cursor + 1, app.len())
    };
    if !app.marks.is_empty() {
        left += &format!("  {} marked", app.marks.len());
    }
    if app.head_only {
        left += "  head only";
    }
    if app.view.first_parent {
        left += "  first parent";
    }
    if let Some(q) = &app.filter {
        left += &format!("  filter: {q} ({} of {})", app.len(), app.total());
    }
    if let Some(q) = &app.search {
        left += &format!("  /{q} ({})", app.search_hits);
    }
    if app.visual.is_some() {
        left += "  -- VISUAL --";
    }
    if let (Some(_), Some(r)) = (app.view.head, app.head_row) {
        left += &format!("  head: {}", app.history.commits[r].short_id());
    }
    if app.history.rebasing {
        left += "  -- REBASE IN PROGRESS --";
    }
    let watch = if app.watch { format!("watch {}s", app.interval.as_secs()) } else { "watch off".into() };
    let hidden = match (app.view.hide.len(), app.view.hiding) {
        (0, _) => String::new(),
        (n, true) => format!("hidden: {n}  "),
        (n, false) => format!("hidden: {n} (shown)  "),
    };
    let mut right = format!(
        "{}{}  deleted: {}  {hidden}{}  ? help",
        app.view.order.name(),
        if app.view.refs_only { "  refs only" } else { "" },
        app.view.deleted.name(),
        watch
    );
    if let Some(m) = &app.message {
        right = format!("{m}  |  {right}");
    }
    let pending = format!(
        "{}{}  {right} ",
        app.count.map(|c| c.to_string()).unwrap_or_default(),
        app.prefix.map(String::from).unwrap_or_default()
    );
    let pad = width.saturating_sub(left.chars().count() + pending.chars().count());
    Line::styled(format!("{left}{}{pending}", " ".repeat(pad)), style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{Commit, Decoration, History, RefKind, View};

    fn app() -> App {
        let commit = |id: u8, summary: &str, parents: Vec<usize>| Commit {
            oid: git2::Oid::from_bytes(&[id; 20]).unwrap(),
            parents,
            author: "Ann".into(),
            date: "2026-10-07".into(),
            summary: summary.into(),
            time: 0,
            merge: false,
            merged: 0,
            deleted: false,
        };
        let mut history = History {
            commits: vec![commit(1, "Update home-lab.md", vec![1]), commit(2, "Add tests", vec![])],
            head: Some(0),
            ..History::default()
        };
        history.decorations.insert(
            0,
            vec![Decoration { kind: RefKind::Head, name: "HEAD -> main".into(), full: None }],
        );
        App::new(history, View::default())
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn row_text_is_what_matching_sees() {
        let a = app();
        for row in 0..2 {
            let line = commit_line(&a, row);
            assert!(text(&line).ends_with(&a.history.line(row)), "{}", text(&line));
        }
    }

    #[test]
    fn merges_get_a_long_dash_with_first_parents_only() {
        let mut a = app();
        a.history.commits[1].merge = true;
        assert!(text(&commit_line(&a, 1)).contains(" - "));
        a.view.first_parent = true;
        let line = text(&commit_line(&a, 1));
        assert!(line.contains("◆") && line.contains(" — "), "{line}");
        assert!(text(&commit_line(&a, 0)).contains(" - "), "not a merge");
    }

    #[test]
    fn merges_with_first_parents_only() {
        let mut a = app();
        a.history.commits[1].merge = true;
        a.history.commits[1].merged = 7;
        a.history.commits[1].summary = "Merge branch 'feature-x' into main".into();
        assert!(!text(&commit_line(&a, 1)).contains("+7"), "all parents shown: nothing added");
        a.view.first_parent = true;
        let shown = text(&commit_line(&a, 1));
        assert!(shown.contains("◆━┄ 0202020 +7 — "), "{shown}");
        // A search still highlights the right characters with the count added.
        a.search = Some("into".into());
        let marked: String = commit_line(&a, 1)
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, "into");
        assert!(!text(&commit_line(&a, 0)).contains('+'), "not a merge");
    }

    #[test]
    fn merge_word_stands_apart() {
        let mut a = app();
        a.history.commits[1].summary = "Merged in feature-x (pull request #7)".into();
        let styled = |a: &App| {
            commit_line(a, 1).spans.iter().find(|s| s.content == "Merged").map(|s| s.style.fg)
        };
        assert_eq!(styled(&a), None, "only for merges");
        a.history.commits[1].merge = true;
        assert_eq!(styled(&a), Some(Some(Color::LightMagenta)));
        assert!(text(&commit_line(&a, 1)).ends_with(&a.history.line(1)), "the text is unchanged");
    }

    #[test]
    fn highlight_marks_the_matched_characters() {
        let mut a = app();
        a.search = Some("hlab".into());
        let line = commit_line(&a, 0);
        let marked: String = line
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Color::Yellow))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, "hlab");
        assert!(text(&line).ends_with(&a.history.line(0)), "splitting keeps the text");
    }
}
