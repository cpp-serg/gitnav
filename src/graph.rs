//! Lane layout for the revision graph: one row per commit, drawn with box-drawing glyphs.
//!
//! The input is, for each row in display order (children before parents), the row indices of
//! that commit's parents. Lanes never shift sideways: a lane keeps its column from the row that
//! opens it to the row that closes it, and freed columns are reused by later lanes.
//!
//! A spine can be given: a line of rows, each a parent of the one before (the layout head's
//! line). Those always take the leftmost lane, which nothing else then uses, so that line runs
//! straight down the left.
//!
//! Two rules keep incoming merges next to the line they merge into, as git's graph does:
//! a line that heads for the same commit as an older line joins it on its own row, freeing its
//! column; and before a merge, a line sitting in the column right of the merge's commit steps
//! one column further right, so the merged branch lands next to the target line.
//!
//! Lines of the head's history (the heavy rows) get the columns next to the spine: a line
//! outside it is placed right of as many columns as the head's history needs anywhere along
//! that line's length.
//!
//! Rows can also be marked heavy (the head and everything it reaches): the lines from a heavy
//! row to its parents are drawn with heavy glyphs. Cells keep the light glyph and say which of
//! their lines are heavy; `shown` picks the glyph to draw.

/// One lane column of one row: the glyph in the lane itself, and the character drawn in the
/// gap to its right (a space, or `─` when a horizontal run passes through).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub glyph: char,
    pub color: u8,
    /// The glyph's vertical line (or the node's row) is heavy.
    pub v_heavy: bool,
    /// The glyph's horizontal line is heavy.
    pub h_heavy: bool,
    pub conn: char,
    pub conn_color: u8,
    pub conn_heavy: bool,
}

impl Cell {
    /// The glyph and the connector to draw, heavy where their lines are.
    pub fn shown(&self) -> (char, char) {
        let (v, h) = (self.v_heavy, self.h_heavy);
        let pick = |both: char, v_only: char, h_only: char| match (v, h) {
            (true, true) => both,
            (true, false) => v_only,
            (false, true) => h_only,
            (false, false) => self.glyph,
        };
        let glyph = match self.glyph {
            '│' if v => '┃',
            '─' if h => '━',
            '┼' => pick('╋', '╂', '┿'),
            '┤' => pick('┫', '┨', '┥'),
            '├' => pick('┣', '┠', '┝'),
            '┴' => pick('┻', '┸', '┷'),
            '┬' => pick('┳', '┰', '┯'),
            '╯' if v || h => '┛',
            '╰' if v || h => '┗',
            '╮' if v || h => '┓',
            '╭' if v || h => '┏',
            g => g,
        };
        (glyph, if self.conn == '─' && self.conn_heavy { '━' } else { self.conn })
    }
}

/// Number of distinct lane colours; the UI maps each index to a terminal colour.
pub const COLORS: u8 = 12;

const BLANK: Cell = Cell {
    glyph: ' ',
    color: 0,
    v_heavy: false,
    h_heavy: false,
    conn: ' ',
    conn_color: 0,
    conn_heavy: false,
};

#[derive(Clone, Copy)]
struct Lane {
    waiting_for: usize,
    color: u8,
    /// Opened by a heavy row: the line to its parent is heavy.
    heavy: bool,
    /// The row the line started at; a younger line joins an older one heading the same way.
    born: usize,
}

/// Lays out the whole graph and returns the cells of each row, trailing blanks trimmed.
/// `spine` lists the rows kept in lane 0, top to bottom; it may be empty. `heavy[row]` marks
/// the rows whose lines to their parents are heavy; it may be shorter than the rows.
pub fn layout<'a>(
    parents: impl IntoIterator<Item = &'a [usize]>,
    spine: &[usize],
    heavy: &[bool],
) -> Vec<Vec<Cell>> {
    // With a spine, lane 0 is for it alone, and only ever waits for the next spine row.
    let first_lane = usize::from(!spine.is_empty());
    let spine_next: std::collections::HashMap<usize, Option<usize>> =
        spine.iter().enumerate().map(|(i, &r)| (r, spine.get(i + 1).copied())).collect();
    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut next_color = 0u8;
    let mut new_color = || {
        let c = next_color;
        next_color = (next_color + 1) % COLORS;
        c
    };
    let mut rows = Vec::new();
    let all: Vec<&[usize]> = parents.into_iter().collect();
    let reserved = Reserved::new(&all, heavy, &spine_next);
    let is_heavy = |r: usize| heavy.get(r).copied().unwrap_or(false);
    // Where a line heading for row p ends: at p if p is in the head's history or on the spine,
    // else wherever p's own line ends, following first parents.
    let mut line_end = vec![0usize; all.len()];
    for r in (0..all.len()).rev() {
        line_end[r] = match all[r].first() {
            Some(&p) if p < all.len() => {
                if is_heavy(p) || spine_next.contains_key(&p) { p } else { line_end[p] }
            }
            _ => r,
        };
    }
    let ends_at = |p: usize| {
        if p >= all.len() || is_heavy(p) || spine_next.contains_key(&p) { p } else { line_end[p] }
    };
    // The first column a line outside the head's history may take, starting at `row` and
    // heading for `p`. Without a spine there is no head line to keep them next to.
    let outer = |row: usize, p: usize| {
        if spine.is_empty() { first_lane } else { first_lane + reserved.max(row, ends_at(p)) }
    };

    for (row, &parents) in all.iter().enumerate() {
        let row_heavy = heavy.get(row).copied().unwrap_or(false);
        let hits: Vec<usize> = (0..lanes.len())
            .filter(|&j| lanes[j].is_some_and(|l| l.waiting_for == row))
            .collect();

        let on_spine = spine_next.get(&row).copied();
        let (col, color) = if on_spine.is_some() {
            (0, lanes.first().copied().flatten().map_or_else(&mut new_color, |l| l.color))
        } else if let Some(&col) = hits.first() {
            (col, lanes[col].unwrap().color)
        } else {
            let from = if row_heavy { first_lane } else { outer(row, parents.first().copied().unwrap_or(row)) };
            let col = (from..lanes.len()).find(|&j| lanes[j].is_none());
            (col.unwrap_or(lanes.len().max(from)), new_color())
        };
        while lanes.len() <= col {
            lanes.push(None);
        }

        let mut cells = vec![BLANK; lanes.len()];
        for (j, lane) in lanes.iter().enumerate() {
            if let Some(l) = lane {
                cells[j] = Cell { glyph: '│', color: l.color, v_heavy: l.heavy, ..BLANK };
            }
        }
        cells[col] = Cell { glyph: '●', color, v_heavy: row_heavy, ..BLANK };

        // (column, colour, heavy) of every lane joined to the commit by a horizontal run.
        let mut ends: Vec<(usize, u8, bool)> = Vec::new();
        for &h in hits.iter().filter(|&&h| h != col) {
            let lane = lanes[h].unwrap();
            cells[h].glyph = if h > col { '╯' } else { '╰' };
            cells[h].h_heavy = lane.heavy;
            ends.push((h, lane.color, lane.heavy));
            lanes[h] = None;
        }
        // The lane goes on to the next spine row, else to the first parent; the other parents
        // branch off it. Off the spine, if an older line already heads for that parent, this
        // one joins it here.
        let main = on_spine.unwrap_or_else(|| parents.first().copied());
        let born = match lanes[col] {
            _ if on_spine.is_some() => 0,
            Some(l) => l.born,
            None => row,
        };
        // A line of the head's history never gives way to one outside it; otherwise the
        // younger line joins the older.
        let older = |l: &Option<Lane>, p: usize| {
            l.is_some_and(|l| {
                l.waiting_for == p && (l.heavy && !row_heavy || l.heavy == row_heavy && l.born < born)
            })
        };
        let join = main
            .filter(|_| on_spine.is_none())
            .and_then(|p| (0..lanes.len()).find(|&j| j != col && older(&lanes[j], p)));
        if let Some(j) = join {
            cells[j].glyph = if j > col { '┤' } else { '├' };
            cells[j].h_heavy = row_heavy;
            ends.push((j, lanes[j].unwrap().color, row_heavy));
            lanes[col] = None;
        } else {
            lanes[col] = main.map(|p| Lane { waiting_for: p, color, heavy: row_heavy, born });
        }

        for &p in parents.iter().filter(|&&p| Some(p) != main) {
            if let Some(j) = lanes.iter().position(|l| l.is_some_and(|l| l.waiting_for == p)) {
                if j != col {
                    cells[j].glyph = if j > col { '┤' } else { '├' };
                    cells[j].h_heavy = row_heavy;
                    ends.push((j, lanes[j].unwrap().color, row_heavy));
                }
                continue;
            }
            // A column freed on this row still shows its merge glyph, so skip those.
            let from = if row_heavy { col + 1 } else { (col + 1).max(outer(row, p)) };
            let free = (from..lanes.len())
                .find(|&j| lanes[j].is_none() && !hits.contains(&j) && cells[j].glyph == ' ');
            let j = free.unwrap_or_else(|| {
                while lanes.len() < from {
                    lanes.push(None);
                    cells.push(BLANK);
                }
                lanes.push(None);
                cells.push(BLANK);
                lanes.len() - 1
            });
            let c = new_color();
            lanes[j] = Some(Lane { waiting_for: p, color: c, heavy: row_heavy, born: row });
            let glyph = if j > col { '╮' } else { '╭' };
            cells[j] = Cell { glyph, color: c, v_heavy: row_heavy, h_heavy: row_heavy, ..BLANK };
            ends.push((j, c, row_heavy));
        }

        for &(end, c, run_heavy) in &ends {
            let (lo, hi) = (col.min(end), col.max(end));
            for (k, cell) in cells.iter_mut().enumerate().take(hi).skip(lo) {
                if k != lo {
                    cell.glyph = crossed(cell.glyph);
                    if cell.glyph == '─' {
                        cell.color = c;
                    }
                    cell.h_heavy |= run_heavy;
                }
                cell.conn = '─';
                cell.conn_color = c;
                cell.conn_heavy |= run_heavy;
            }
        }

        if let Some(&next) = all.get(row + 1) {
            make_room_for_merge(&mut lanes, &mut cells, row + 1, next, &spine_next, first_lane);
        }

        while cells.last().is_some_and(|c| *c == BLANK) {
            cells.pop();
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(cells);
    }
    rows
}

/// How many lines of the head's history (heavy rows to their parents, the spine aside) are
/// open at each row, with range maximums over rows.
struct Reserved {
    /// `table[k][i]`: the maximum over rows i..i + 2^k.
    table: Vec<Vec<usize>>,
}

impl Reserved {
    fn new(
        parents: &[&[usize]],
        heavy: &[bool],
        spine_next: &std::collections::HashMap<usize, Option<usize>>,
    ) -> Self {
        let n = parents.len();
        let mut diff = vec![0isize; n + 1];
        for (r, ps) in parents.iter().enumerate().filter(|&(r, _)| heavy.get(r).copied().unwrap_or(false)) {
            // Rows r..p: at p the line ends in p's own lane.
            for &p in ps.iter().filter(|&&p| p < n && spine_next.get(&r) != Some(&Some(p))) {
                diff[r] += 1;
                diff[p] -= 1;
            }
        }
        let mut open = Vec::with_capacity(n);
        let mut sum = 0isize;
        for d in &diff[..n] {
            sum += d;
            open.push(sum.max(0) as usize);
        }
        let mut table = vec![open];
        let mut width = 1;
        while width * 2 <= n {
            let prev = table.last().unwrap();
            let next = (0..=n - width * 2).map(|i| prev[i].max(prev[i + width])).collect();
            table.push(next);
            width *= 2;
        }
        Reserved { table }
    }

    /// The most lines open at any row in a..=b.
    fn max(&self, a: usize, b: usize) -> usize {
        let (a, b) = (a.min(b), a.max(b).min(self.table[0].len().saturating_sub(1)));
        if self.table[0].is_empty() || a > b {
            return 0;
        }
        let k = (usize::BITS - 1 - (b - a + 1).leading_zeros()) as usize;
        self.table[k][a].max(self.table[k][b + 1 - (1 << k)])
    }
}

/// If row `next` is a merge whose merged branch will need a new lane, and the column right of
/// where `next` will sit holds a line passing by, that line steps one column right on the
/// current row (`cells`): `╰─╮`, or `●─╮` from the current row's own commit. Only when that is
/// clean: the column beyond is free, and the line's cell has nothing else in it.
fn make_room_for_merge(
    lanes: &mut Vec<Option<Lane>>,
    cells: &mut Vec<Cell>,
    next: usize,
    parents: &[usize],
    spine_next: &std::collections::HashMap<usize, Option<usize>>,
    first_lane: usize,
) {
    if parents.len() < 2 {
        return;
    }
    let waiting = |p: usize| lanes.iter().position(|l| l.is_some_and(|l| l.waiting_for == p));
    let hits: Vec<usize> = (0..lanes.len()).filter(|&j| lanes[j].is_some_and(|l| l.waiting_for == next)).collect();
    let col = if spine_next.contains_key(&next) {
        0
    } else if let Some(&h) = hits.first() {
        h
    } else {
        (first_lane..lanes.len()).find(|&j| lanes[j].is_none()).unwrap_or(lanes.len().max(first_lane))
    };
    let main = spine_next.get(&next).copied().unwrap_or_else(|| parents.first().copied());
    if !parents.iter().any(|&p| Some(p) != main && waiting(p).is_none()) {
        return;
    }
    let t = col + 1;
    let Some(Some(lane)) = lanes.get(t).copied() else { return };
    let beyond_free = lanes.get(t + 1).is_none_or(Option::is_none) && cells.get(t + 1).is_none_or(|c| *c == BLANK);
    let clean = cells.get(t).is_some_and(|c| c.conn == ' ' && (c.glyph == '│' || c.glyph == '●'));
    if hits.contains(&t) || !beyond_free || !clean {
        return;
    }
    while lanes.len() <= t + 1 {
        lanes.push(None);
    }
    while cells.len() <= t + 1 {
        cells.push(BLANK);
    }
    lanes[t + 1] = lanes[t].take();
    if cells[t].glyph == '│' {
        cells[t].glyph = '╰';
        cells[t].h_heavy = lane.heavy;
    }
    cells[t].conn = '─';
    cells[t].conn_color = lane.color;
    cells[t].conn_heavy = lane.heavy;
    cells[t + 1] = Cell { glyph: '╮', color: lane.color, v_heavy: lane.heavy, h_heavy: lane.heavy, ..BLANK };
}

/// The glyph a cell shows when a horizontal run passes through it.
fn crossed(glyph: char) -> char {
    match glyph {
        ' ' => '─',
        '│' | '┤' | '├' => '┼',
        '╯' | '╰' => '┴',
        '╮' | '╭' => '┬',
        g => g,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(parents: &[&[usize]]) -> String {
        layout(parents.iter().copied(), &[], &[])
            .iter()
            .map(|row| {
                let s: String = row.iter().flat_map(|c| [c.glyph, c.conn]).collect();
                s.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn linear() {
        assert_eq!(draw(&[&[1], &[2], &[]]), "●\n●\n●");
    }

    #[test]
    fn fork_and_merge() {
        // 0 merges 1 and 2, both of which fork from 3.
        let got = draw(&[&[1, 2], &[3], &[3], &[]]);
        assert_eq!(got, "●─╮\n● │\n│ ●\n●─╯");
    }

    #[test]
    fn two_tips_join() {
        // Two branch tips (0, 1) with a common parent 2: the younger joins the older line.
        assert_eq!(draw(&[&[2], &[2], &[]]), "●\n├─●\n●");
    }

    #[test]
    fn octopus() {
        let got = draw(&[&[1, 2, 3], &[4], &[4], &[4], &[]]);
        assert_eq!(got, "●─┬─╮\n● │ │\n│ ● │\n│ │ ●\n●─┴─╯");
    }

    #[test]
    fn two_roots() {
        assert_eq!(draw(&[&[1], &[], &[3], &[]]), "●\n●\n●\n●");
    }

    #[test]
    fn criss_cross() {
        // 0 and 1 are both merges of 2 and 3.
        let got = draw(&[&[2, 3], &[3, 2], &[4], &[4], &[]]);
        assert_eq!(got, "●─╮\n├─┼─●\n● │\n│ ●\n●─╯");
    }

    #[test]
    fn merge_into_existing_lane() {
        // 0 is a tip on 2; 1 merges 3 and 2, so its second parent joins lane 0.
        let got = draw(&[&[2], &[3, 2], &[3], &[]]);
        assert_eq!(got, "●\n├─●\n● │\n●─╯");
    }

    #[test]
    fn missing_parent_ends_lane() {
        // Row 1's parent is outside the history, so it has none here.
        assert_eq!(draw(&[&[1], &[]]), "●\n●");
    }

    #[test]
    fn spine_keeps_the_left_lane() {
        // 0 is a newer tip on 2; 1 is the layout head, also on 2.
        let parents: &[&[usize]] = &[&[2], &[2], &[]];
        // Without a spine the younger tip joins the older line on its own row.
        let plain: Vec<String> = render(parents, &[]);
        assert_eq!(plain, ["●", "├─●", "●"]);
        // With 1 and 2 as the spine, the tip moves right and lane 0 waits for the head.
        assert_eq!(render(parents, &[1, 2]), ["  ●", "● │", "●─╯"]);
    }

    #[test]
    fn spine_takes_merges_from_the_right() {
        // The head 0 merges 2 (first parent) and 1; 1 and 2 fork from 3.
        let parents: &[&[usize]] = &[&[2, 1], &[3], &[3], &[]];
        assert_eq!(render(parents, &[0, 2, 3]), ["●─╮", "│ ●", "● │", "●─╯"]);
    }

    #[test]
    fn spine_can_follow_a_second_parent() {
        // 0 merges 1 (first parent) and 2; the spine goes 0 -> 2 -> 3, so 1 branches off.
        let parents: &[&[usize]] = &[&[1, 2], &[3], &[3], &[]];
        assert_eq!(render(parents, &[0, 2, 3]), ["●─╮", "│ ●", "● │", "●─╯"]);
        // Without a spine the first parent stays in line.
        assert_eq!(render(parents, &[]), ["●─╮", "● │", "│ ●", "●─╯"]);
    }

    fn render(parents: &[&[usize]], spine: &[usize]) -> Vec<String> {
        layout(parents.iter().copied(), spine, &[])
            .iter()
            .map(|row| row.iter().flat_map(|c| [c.glyph, c.conn]).collect::<String>().trim_end().to_string())
            .collect()
    }

    /// The glyphs as drawn, heavy ones included.
    fn render_heavy(parents: &[&[usize]], spine: &[usize], heavy: &[bool]) -> Vec<String> {
        layout(parents.iter().copied(), spine, heavy)
            .iter()
            .map(|row| {
                let s: String = row.iter().flat_map(|c| { let (g, k) = c.shown(); [g, k] }).collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn heavy_lines_from_heavy_rows() {
        // 0 merges 1 and 2, both on 3. Heavy from 1 down (1 and 3): 0's lines stay light.
        let parents: &[&[usize]] = &[&[1, 2], &[3], &[3], &[]];
        let heavy = [false, true, false, true];
        // 2's light line joins the heavy one heading for 3.
        assert_eq!(render_heavy(parents, &[], &heavy), ["●─╮", "● │", "┠─●", "●"]);
        // Heavy from 0: everything is.
        let heavy = [true; 4];
        assert_eq!(render_heavy(parents, &[], &heavy), ["●━┓", "● ┃", "┃ ●", "●━┛"]);
    }

    #[test]
    fn heavy_and_light_lines_cross() {
        // The criss-cross: light row 1 joins lane 0 (0's heavy line to 2) with a run that
        // passes over lane 1 (0's heavy line to 3).
        let parents: &[&[usize]] = &[&[2, 3], &[3, 2], &[4], &[4], &[]];
        let heavy = [true, false, true, true, true];
        let rows = render_heavy(parents, &[], &heavy);
        assert_eq!(rows[1], "┠─╂─●", "light joins and crosses heavy lanes");
    }

    #[test]
    fn merges_land_next_to_the_target_line() {
        // The repo from the discussion: main merges y, then s, then x; all fork from base.
        // Rows: 0 merge y [3, 1]; 1 y2 [2]; 2 y1 [12]; 3 merge s [7, 4]; 4 s3 [5]; 5 s2 [6];
        // 6 s1 [12]; 7 m2 [8]; 8 merge x [11, 9]; 9 x2 [10]; 10 x1 [12]; 11 m1 [12]; 12 base.
        let parents: &[&[usize]] = &[
            &[3, 1], &[2], &[12], &[7, 4], &[5], &[6], &[12], &[8], &[11, 9], &[10], &[12], &[12], &[],
        ];
        let spine = [0, 3, 7, 8, 11, 12];
        assert_eq!(
            render(parents, &spine),
            [
                "●─╮", "│ ●", "│ ●─╮", "●─╮ │", "│ ● │", "│ ● │", "│ ●─┤", "●   │", "●─╮ │", "│ ● │",
                "│ ●─┤", "●   │", "●───╯",
            ]
        );
    }

    #[test]
    fn head_history_gets_the_columns_next_to_the_spine() {
        // Row 0 is a tip outside the head's history, heading for the root 4. The head 1 merges
        // 3 (spine) and 2, which is in its history.
        let parents: &[&[usize]] = &[&[4], &[3, 2], &[3], &[4], &[]];
        let spine = [1, 3, 4];
        // Unmarked, the tip takes column 1 and has to step aside for the merge.
        assert_eq!(render(parents, &spine), ["  ●─╮", "●─╮ │", "├─● │", "●   │", "●───╯"]);
        // With the head's history marked, the tip starts right of the column it needs.
        let heavy = [false, true, true, true, true];
        assert_eq!(render_heavy(parents, &spine, &heavy), ["    ●", "●━┓ │", "┣━● │", "●   │", "●───╯"]);
    }

    #[test]
    fn head_history_does_not_join_lines_outside_it() {
        // Row 0 is a tip outside the head's history, on 3. The head 1 merges 4 (spine) and 2,
        // and 2 is also on 3. The head's line through 2 keeps its column; the outside line
        // merges into 3 when it gets there.
        let parents: &[&[usize]] = &[&[3], &[4, 2], &[3], &[4], &[]];
        let spine = [1, 4];
        let heavy = [false, true, true, true, true];
        // (3 also joins the spine early, taking the outside line in on the same row.)
        assert_eq!(render_heavy(parents, &spine, &heavy), ["    ●", "●━┓ │", "┃ ● │", "┣━●─╯", "●"]);
        // Unmarked, the younger line through 2 joins the older one.
        assert_eq!(render(parents, &spine)[2], "│ ●─┤");
    }

    #[test]
    fn run_over_freed_lane() {
        // 4's run to lane 2 passes over lane 1, freed when 3 merged it.
        let got = draw(&[&[3], &[2, 4], &[3], &[4], &[]]);
        assert_eq!(got, "●\n│ ●─╮\n├─● │\n●   │\n●───╯");
    }
}
