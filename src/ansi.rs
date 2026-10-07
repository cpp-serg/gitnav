//! Turns text with ANSI colour codes (git's `--color=always` output, or a pager's such as
//! delta's) into styled lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const TAB: usize = 8;

/// One `Line` per input line. SGR sequences (`ESC [ … m`) set the style; other escape
/// sequences are dropped; tabs become spaces up to the next multiple of 8 columns.
pub fn to_lines(text: &str) -> Vec<Line<'static>> {
    let mut style = Style::new();
    text.lines().map(|line| parse_line(line, &mut style)).collect()
}

/// Parses one line; `style` carries over, since a colour may span lines.
fn parse_line(line: &str, style: &mut Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut column = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' if chars.peek() == Some(&']') => {
                // An OSC sequence (a hyperlink, say), ended by BEL or ESC \: not shown.
                while let Some(c) = chars.next() {
                    if c == '\x07' || c == '\x1b' && chars.next_if_eq(&'\\').is_some() {
                        break;
                    }
                }
            }
            '\x1b' => {
                if chars.peek() != Some(&'[') {
                    continue;
                }
                chars.next();
                let mut params = String::new();
                let mut end = None;
                for c in chars.by_ref() {
                    if c.is_ascii_digit() || c == ';' {
                        params.push(c);
                    } else {
                        end = Some(c);
                        break;
                    }
                }
                if end == Some('m') {
                    if !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), *style));
                    }
                    apply_sgr(style, &params);
                }
            }
            '\t' => {
                let spaces = TAB - column % TAB;
                run.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\r' => {}
            c => {
                run.push(c);
                column += 1;
            }
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, *style));
    }
    Line::from(spans)
}

fn apply_sgr(style: &mut Style, params: &str) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::new(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            5 => *style = style.add_modifier(Modifier::SLOW_BLINK),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            25 => *style = style.remove_modifier(Modifier::SLOW_BLINK),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            c @ 30..=37 => *style = style.fg(Color::Indexed((c - 30) as u8)),
            c @ 90..=97 => *style = style.fg(Color::Indexed((c - 90 + 8) as u8)),
            c @ 40..=47 => *style = style.bg(Color::Indexed((c - 40) as u8)),
            c @ 100..=107 => *style = style.bg(Color::Indexed((c - 100 + 8) as u8)),
            39 => style.fg = Some(Color::Reset),
            49 => style.bg = Some(Color::Reset),
            c @ (38 | 48) => {
                // 38;5;n or 38;2;r;g;b (and 48 for the background).
                let color = match codes.get(i + 1) {
                    Some(5) => codes.get(i + 2).map(|&n| (Color::Indexed(n as u8), 2)),
                    Some(2) => match (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4)) {
                        (Some(&r), Some(&g), Some(&b)) => Some((Color::Rgb(r as u8, g as u8, b as u8), 4)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((color, used)) = color {
                    *style = if c == 38 { style.fg(color) } else { style.bg(color) };
                    i += used;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn colours_and_resets() {
        let lines = to_lines("\x1b[33mcommit abc\x1b[m\n\x1b[1;32m+added\x1b[m\nplain");
        assert_eq!(lines.len(), 3);
        assert_eq!(plain(&lines[0]), "commit abc");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(3)));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Indexed(2)));
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[2].spans[0].style, Style::new());
    }

    #[test]
    fn extended_colours() {
        let lines = to_lines("\x1b[38;5;208mx\x1b[48;2;1;2;3my");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(lines[0].spans[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn tabs_and_other_escapes() {
        let lines = to_lines("a\tb\x1b[Kc\r");
        assert_eq!(plain(&lines[0]), "a       bc");
    }

    #[test]
    fn hyperlinks_are_dropped() {
        let lines = to_lines("\x1b]8;;file:///a.rs\x1b\\a.rs\x1b]8;;\x1b\\ and \x1b]8;;x\x07b\x1b]8;;\x07");
        assert_eq!(plain(&lines[0]), "a.rs and b");
    }

    #[test]
    fn style_carries_over_lines() {
        let lines = to_lines("\x1b[31mred\nstill red\x1b[m");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Indexed(1)));
    }
}
