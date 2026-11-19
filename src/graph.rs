//! Lane layout for the revision graph: one line per commit, and between commits the lines
//! where the graph's lines change column, drawn with box-drawing glyphs.
//!
//! The input is, for each row in display order (children before parents), the row indices of
//! that commit's parents. Lanes never shift sideways: a lane keeps its column from the row that
//! opens it to the row that closes it, and freed columns are reused by later lanes.
//!
//! As in `git log --graph`, a line changes column between commits, on connector lines under
//! the commit above: a step to the next column is one `╲` or `╱` in the gap between the two.
//! A longer reach is a staircase of diagonals, one character sideways per line, over as many
//! connector lines as it takes; a line passing by may begin it on the commit's line, in the
//! gap beside its column. Only where a staircase would cut through another line does the reach
//! run along `_` from one diagonal to another, as git draws it.
//!
//! A spine can be given: a line of rows, each a parent of the one before (the layout head's
//! line). Those always take the leftmost lane, which nothing else then uses, so that line runs
//! straight down the left.
//!
//! Two rules keep incoming merges next to the line they merge into, as git's graph does:
//! a line that heads for the same commit as an older line joins it under its own row, freeing
//! its column; and before a merge, a line sitting in the column right of the merge's commit
//! steps one column further right, so the merged branch lands next to the target line.
//!
//! Lines of the head's history (the heavy rows) get the columns next to the spine: a line
//! outside it is placed right of as many columns as the head's history needs anywhere along
//! that line's length.
//!
//! Rows can also be marked heavy (the head and everything it reaches): the lines from a heavy
//! row to its parents are drawn with heavy glyphs. Cells keep the light glyph and say which of
//! their lines are heavy; `shown` picks the glyph to draw. There are no heavy diagonals, so
//! those are drawn bold instead.

/// One lane column of one line: the glyph in the lane itself, and the character drawn in the
/// gap to its right (a space, a diagonal, or `_` where a long reach passes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub glyph: char,
    pub color: u8,
    /// The glyph's vertical line (or the node's row) is heavy.
    pub v_heavy: bool,
    /// The glyph's horizontal or diagonal line is heavy.
    pub h_heavy: bool,
    pub conn: char,
    pub conn_color: u8,
    pub conn_heavy: bool,
}

impl Cell {
    /// The glyph and the connector to draw, heavy where their lines are.
    pub fn shown(&self) -> (char, char) {
        let glyph = if self.glyph == '│' && self.v_heavy { '┃' } else { self.glyph };
        (glyph, self.conn)
    }
}

/// One commit's lines: its own, then the connector lines leading to the next commit's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub below: Vec<Vec<Cell>>,
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
    /// The column the line leaves the current row from, when that is not the lane's own: the
    /// row's commit for a line it starts, or the column the line stepped aside from.
    leaves: Option<usize>,
}

/// A line between two consecutive rows, from a column of the upper to one of the lower.
#[derive(Clone, Copy)]
struct Edge {
    from: usize,
    to: usize,
    color: u8,
    heavy: bool,
    /// The line starts at the upper row's commit, so its diagonal comes first.
    from_node: bool,
}

/// Lays out the whole graph and returns each row's lines, trailing blanks trimmed.
/// `spine` lists the rows kept in lane 0, top to bottom; it may be empty. `heavy[row]` marks
/// the rows whose lines to their parents are heavy; it may be shorter than the rows.
pub fn layout<'a>(
    parents: impl IntoIterator<Item = &'a [usize]>,
    spine: &[usize],
    heavy: &[bool],
) -> Vec<Row> {
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
    let mut rows: Vec<Row> = Vec::new();
    // The previous row's commit column, and its lines into lanes already open.
    let mut prev_col = None;
    let mut joins: Vec<Edge> = Vec::new();
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

        // The connector from the previous row: every open lane goes on in its column, except
        // the lines ending here, which turn into this commit.
        if let Some(above) = rows.last_mut() {
            let into = |j: usize| if hits.contains(&j) { col } else { j };
            let mut edges: Vec<Edge> = lanes
                .iter_mut()
                .enumerate()
                .filter_map(|(j, l)| {
                    let l = l.as_mut()?;
                    let from = l.leaves.take().unwrap_or(j);
                    Some(Edge { from, to: into(j), color: l.color, heavy: l.heavy, from_node: Some(from) == prev_col })
                })
                .collect();
            edges.extend(joins.drain(..).map(|e| Edge { to: into(e.to), ..e }));
            above.below = connector(&edges, &mut above.cells);
        }

        let mut cells = vec![BLANK; lanes.len()];
        for (j, lane) in lanes.iter().enumerate() {
            if let Some(l) = lane.filter(|_| !hits.contains(&j)) {
                cells[j] = Cell { glyph: '│', color: l.color, v_heavy: l.heavy, ..BLANK };
            }
        }
        cells[col] = Cell { glyph: '●', color, v_heavy: row_heavy, ..BLANK };

        // Every other column the commit is joined to: lines ending here, lines it joins, lines
        // it starts. The step aside for a merge on the next row keeps clear of them.
        let mut ends: Vec<usize> = Vec::new();
        for &h in hits.iter().filter(|&&h| h != col) {
            ends.push(h);
            lanes[h] = None;
        }
        // The lane goes on to the next spine row, else to the first parent; the other parents
        // branch off it. Off the spine, if an older line already heads for that parent, this
        // one joins it under this row.
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
        let join_edge = |to: usize| Edge { from: col, to, color, heavy: row_heavy, from_node: true };
        if let Some(j) = join {
            joins.push(join_edge(j));
            ends.push(j);
            lanes[col] = None;
        } else {
            lanes[col] = main.map(|p| Lane { waiting_for: p, color, heavy: row_heavy, born, leaves: None });
        }

        for &p in parents.iter().filter(|&&p| Some(p) != main) {
            if let Some(j) = lanes.iter().position(|l| l.is_some_and(|l| l.waiting_for == p)) {
                if j != col {
                    joins.push(join_edge(j));
                    ends.push(j);
                }
                continue;
            }
            // A column freed on this row has its line turning into the commit, so skip those.
            let from = if row_heavy { col + 1 } else { (col + 1).max(outer(row, p)) };
            let free = (from..lanes.len()).find(|&j| lanes[j].is_none() && !hits.contains(&j));
            let j = free.unwrap_or_else(|| {
                while lanes.len() < from {
                    lanes.push(None);
                }
                lanes.push(None);
                lanes.len() - 1
            });
            let c = new_color();
            lanes[j] = Some(Lane { waiting_for: p, color: c, heavy: row_heavy, born: row, leaves: Some(col) });
            ends.push(j);
        }

        if let Some(&next) = all.get(row + 1) {
            make_room_for_merge(&mut lanes, row + 1, next, &spine_next, first_lane, col, &hits, &ends);
        }

        while cells.last().is_some_and(|c| *c == BLANK) {
            cells.pop();
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(Row { cells, below: Vec::new() });
        prev_col = Some(col);
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
/// where `next` will sit holds a line passing by, that line steps one column right under the
/// current row. Only when that is clean: the column beyond is free, and none of the current
/// row's own lines (from its commit at `col` to each of `ends`) reaches the line's column or
/// the one beyond.
#[allow(clippy::too_many_arguments)]
fn make_room_for_merge(
    lanes: &mut Vec<Option<Lane>>,
    next: usize,
    parents: &[usize],
    spine_next: &std::collections::HashMap<usize, Option<usize>>,
    first_lane: usize,
    col: usize,
    hits: &[usize],
    ends: &[usize],
) {
    if parents.len() < 2 {
        return;
    }
    let waiting = |p: usize| lanes.iter().position(|l| l.is_some_and(|l| l.waiting_for == p));
    let next_hits: Vec<usize> =
        (0..lanes.len()).filter(|&j| lanes[j].is_some_and(|l| l.waiting_for == next)).collect();
    let next_col = if spine_next.contains_key(&next) {
        0
    } else if let Some(&h) = next_hits.first() {
        h
    } else {
        (first_lane..lanes.len()).find(|&j| lanes[j].is_none()).unwrap_or(lanes.len().max(first_lane))
    };
    let main = spine_next.get(&next).copied().unwrap_or_else(|| parents.first().copied());
    if !parents.iter().any(|&p| Some(p) != main && waiting(p).is_none()) {
        return;
    }
    let t = next_col + 1;
    if !lanes.get(t).is_some_and(Option::is_some) || next_hits.contains(&t) {
        return;
    }
    // Whether a line of the current row covers column k, ends included, or the gap right of it.
    let covers = |k: usize| ends.iter().any(|&e| col.min(e) <= k && k <= col.max(e));
    let covers_gap = |k: usize| ends.iter().any(|&e| col.min(e) <= k && k < col.max(e));
    let beyond_free = lanes.get(t + 1).is_none_or(Option::is_none)
        && t + 1 != col
        && !hits.contains(&(t + 1))
        && !covers(t + 1);
    let clean = !ends.contains(&t) && !covers_gap(t);
    if !beyond_free || !clean {
        return;
    }
    while lanes.len() <= t + 1 {
        lanes.push(None);
    }
    let mut lane = lanes[t].take().unwrap();
    lane.leaves = lane.leaves.or(Some(t));
    lanes[t + 1] = Some(lane);
}

/// The connector lines between two rows, for lines going from a column of the upper row to one
/// of the lower: none when every line goes straight down, one for steps to the next column,
/// more for longer reaches. A step's diagonal is on the first line for a line leaving the upper
/// commit, else on the last, next to where it arrives.
///
/// A longer reach is a staircase, one character sideways per line, and not git's `_` run
/// between two diagonals. A line passing the upper commit by may begin it on that commit's own
/// line (`above`), in the gap beside its column, which saves a connector line. Where the
/// staircase would cut through another line, the reach falls back to the `_` run.
fn connector(edges: &[Edge], above: &mut Vec<Cell>) -> Vec<Vec<Cell>> {
    // Light lines first, so heavy ones win where they share a character.
    let mut edges = edges.to_vec();
    edges.sort_by_key(|e| e.heavy);
    // Long reaches drawn as `_` runs, which need only two lines: those whose staircase did not
    // fit. Fewer lines can make another staircase not fit, so this repeats until it settles.
    let mut runs = vec![false; edges.len()];
    loop {
        let mut upper = above.clone();
        let (lines, fell_back) = draw_connector(&edges, &runs, &mut upper);
        if fell_back.iter().zip(&runs).all(|(&f, &r)| !f || r) {
            *above = upper;
            return lines;
        }
        for (r, f) in runs.iter_mut().zip(fell_back) {
            *r |= f;
        }
    }
}

/// One go at the connector lines, with the long reaches marked in `runs` drawn as `_` runs.
/// Also says which long reaches had to fall back to a run.
fn draw_connector(edges: &[Edge], runs: &[bool], above: &mut Vec<Cell>) -> (Vec<Vec<Cell>>, Vec<bool>) {
    let reach = |e: &Edge| e.from.abs_diff(e.to);
    // Whether a long reach can begin on the upper commit's line: it passes by there.
    let starts_above = |e: &Edge| !e.from_node && above.get(e.from).is_some_and(|c| c.glyph == '│');
    let need = |(e, &run): (&Edge, &bool)| match reach(e) {
        0 => 0,
        1 => 1,
        _ if run => 2,
        r if starts_above(e) => 2 * r - 2,
        r => 2 * r - 1,
    };
    let height = edges.iter().zip(runs).map(need).max().unwrap_or(0);
    let mut fell_back = vec![false; edges.len()];
    if height == 0 {
        return (Vec::new(), fell_back);
    }
    let width = edges.iter().map(|e| e.from.max(e.to) + 1).max().unwrap_or(0);
    let mut lines = vec![vec![BLANK; width]; height];
    // Verticals, then steps, then long reaches.
    for e in edges.iter().filter(|e| e.from == e.to) {
        for line in &mut lines {
            put(line, 2 * e.from, '│', e);
        }
    }
    for e in edges.iter().filter(|e| reach(e) == 1) {
        let (a, b) = (e.from, e.to);
        let at = if e.from_node { 0 } else { height - 1 };
        for (i, line) in lines.iter_mut().enumerate() {
            match i.cmp(&at) {
                std::cmp::Ordering::Less => put(line, 2 * a, '│', e),
                std::cmp::Ordering::Equal => put(line, 2 * a.min(b) + 1, if b > a { '╲' } else { '╱' }, e),
                std::cmp::Ordering::Greater => put(line, 2 * b, '│', e),
            }
        }
    }
    for (k, e) in edges.iter().enumerate().filter(|(_, e)| reach(e) > 1) {
        if runs[k] || !staircase(e, &mut lines, above) {
            fell_back[k] = !runs[k];
            run(e, &mut lines);
        }
    }
    for line in lines.iter_mut().chain(std::iter::once(above)) {
        while line.last().is_some_and(|c| *c == BLANK) {
            line.pop();
        }
    }
    (lines, fell_back)
}

/// Draws `ch` for line `e` at character position `x` of `line` (even: a lane, odd: the gap
/// right of one), where nothing else is drawn or where it joins what is.
fn put(line: &mut [Cell], x: usize, ch: char, e: &Edge) {
    let cell = &mut line[x / 2];
    if x.is_multiple_of(2) {
        match ch {
            // Two lines down the same lane: a heavy one wins.
            '│' if cell.glyph == '│' && (cell.v_heavy || !e.heavy) => {}
            '│' => *cell = Cell { glyph: '│', color: e.color, v_heavy: e.heavy, ..*cell },
            _ if cell.glyph == ' ' || cell.glyph == '_' => {
                *cell = Cell { glyph: ch, color: e.color, h_heavy: e.heavy, ..*cell }
            }
            _ => {}
        }
    } else {
        let ch = match (cell.conn, ch) {
            ('╱', '╲') | ('╲', '╱') | ('╳', _) => '╳',
            ('╱' | '╲', '_') => return,
            _ => ch,
        };
        *cell = Cell { conn: ch, conn_color: e.color, conn_heavy: e.heavy, ..*cell };
    }
}

/// Whether `ch` can go at position `x` of `line` without covering another line: the spot is
/// free, or holds the same diagonal (two lines for the same commit meet there).
fn fits(line: &[Cell], x: usize, ch: char) -> bool {
    let Some(cell) = line.get(x / 2) else { return x / 2 >= line.len() };
    if x.is_multiple_of(2) { cell.glyph == ' ' || cell.glyph == ch } else { cell.conn == ' ' || cell.conn == ch }
}

/// A long reach as a staircase: one diagonal character per line, going one position sideways
/// each time. A line leaving the upper commit begins at once and then goes straight down; a
/// line passing by goes straight down first and arrives on the last line, beginning on the
/// upper commit's line if the connector lines are too few. False, with nothing drawn, if it
/// would cover another line.
fn staircase(e: &Edge, lines: &mut [Vec<Cell>], above: &mut Vec<Cell>) -> bool {
    let (a, b, height) = (e.from, e.to, lines.len() as isize);
    let steps = 2 * a.abs_diff(b) as isize - 1;
    let ch = if b < a { '╱' } else { '╲' };
    let x_at = |k: isize| (2 * a as isize + if b < a { -k } else { k }) as usize;
    // The line index of the k-th diagonal (1-based); -1 is the upper commit's own line.
    let first = if e.from_node { 0 } else { height - steps };
    if first < -1 || first == -1 && !above.get(a).is_some_and(|c| c.glyph == '│') {
        return false;
    }
    let line_of = |i: isize| -> Option<&Vec<Cell>> { if i < 0 { None } else { Some(&lines[i as usize]) } };
    let diagonal_fits = (1..=steps).all(|k| {
        let i = first + k - 1;
        match line_of(i) {
            Some(line) => fits(line, x_at(k), ch),
            None => fits(above, x_at(k), ch),
        }
    });
    let straight: Vec<(usize, usize)> = (0..height as usize)
        .filter(|&i| (i as isize) < first || (i as isize) > first + steps - 1)
        .map(|i| (i, if (i as isize) < first { a } else { b }))
        .collect();
    let straight_fits = straight.iter().all(|&(i, col)| fits(&lines[i], 2 * col, '│'));
    if !diagonal_fits || !straight_fits {
        return false;
    }
    for &(i, col) in &straight {
        put(&mut lines[i], 2 * col, '│', e);
    }
    for k in 1..=steps {
        let i = first + k - 1;
        let x = x_at(k);
        if i < 0 {
            // It leaves its column on the commit's line: the column is empty there now.
            above[a].glyph = ' ';
            while above.len() <= x / 2 {
                above.push(BLANK);
            }
            put(above, x, ch, e);
        } else {
            put(&mut lines[i as usize], x, ch, e);
        }
    }
    true
}

/// A long reach as git draws it: along `_` from a diagonal at one end to a diagonal on the next
/// line at the other, on the first two connector lines for a line leaving the upper commit,
/// else on the last two; straight down on any others.
fn run(e: &Edge, lines: &mut [Vec<Cell>]) {
    let (a, b) = (e.from, e.to);
    let (lo, hi) = (a.min(b), a.max(b));
    let top = if e.from_node { 0 } else { lines.len() - 2 };
    for (i, line) in lines.iter_mut().enumerate() {
        if i < top {
            put(line, 2 * a, '│', e);
        } else if i > top + 1 {
            put(line, 2 * b, '│', e);
        }
    }
    for x in 2 * lo + 2..=2 * hi - 2 {
        put(&mut lines[top], x, '_', e);
    }
    if b > a {
        put(&mut lines[top], 2 * a + 1, '╲', e);
        put(&mut lines[top + 1], 2 * b - 1, '╲', e);
    } else {
        put(&mut lines[top], 2 * a - 1, '╱', e);
        put(&mut lines[top + 1], 2 * b + 1, '╱', e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line of the graph, each commit's followed by its connector lines.
    fn lines(rows: &[Row], text: impl Fn(&Cell) -> (char, char)) -> Vec<String> {
        rows.iter()
            .flat_map(|row| std::iter::once(&row.cells).chain(&row.below))
            .map(|cells| {
                let s: String = cells.iter().flat_map(|c| { let (g, k) = text(c); [g, k] }).collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    fn draw(parents: &[&[usize]]) -> String {
        render(parents, &[]).join("\n")
    }

    #[test]
    fn linear() {
        assert_eq!(draw(&[&[1], &[2], &[]]), "●\n●\n●");
    }

    #[test]
    fn fork_and_merge() {
        // 0 merges 1 and 2, both of which fork from 3.
        let got = render(&[&[1, 2], &[3], &[3], &[]], &[]);
        assert_eq!(got, ["●", "│╲", "● │", "│ ●", "│╱", "●"]);
    }

    #[test]
    fn two_tips_join() {
        // Two branch tips (0, 1) with a common parent 2: the younger joins the older line.
        assert_eq!(render(&[&[2], &[2], &[]], &[]), ["●", "│ ●", "│╱", "●"]);
    }

    #[test]
    fn octopus() {
        // The third parent is two columns away: it reaches there along `_`.
        let got = render(&[&[1, 2, 3], &[4], &[4], &[4], &[]], &[]);
        assert_eq!(got, ["●", "│╲_", "│ │╲", "● │ │", "│ ● │", "│ │ ●", "│ │╱", "│╱", "●"]);
    }

    #[test]
    fn two_roots() {
        assert_eq!(draw(&[&[1], &[], &[3], &[]]), "●\n●\n●\n●");
    }

    #[test]
    fn criss_cross() {
        // 0 and 1 are both merges of 2 and 3.
        // 1's line to 2 crosses 0's line to 3, as in git's `|/|`.
        let got = render(&[&[2, 3], &[3, 2], &[4], &[4], &[]], &[]);
        assert_eq!(got, ["●", "│╲", "│ │ ●", "│ │╱", "│╱│", "● │", "│ ●", "│╱", "●"]);
    }

    #[test]
    fn merge_into_existing_lane() {
        // 0 is a tip on 2; 1 merges 3 and 2, so its second parent joins lane 0.
        let got = render(&[&[2], &[3, 2], &[3], &[]], &[]);
        assert_eq!(got, ["●", "│ ●", "│╱│", "● │", "│╱", "●"]);
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
        // Without a spine the younger tip joins the older line under its own row.
        let plain: Vec<String> = render(parents, &[]);
        assert_eq!(plain, ["●", "│ ●", "│╱", "●"]);
        // With 1 and 2 as the spine, the tip moves right and lane 0 waits for the head.
        assert_eq!(render(parents, &[1, 2]), ["  ●", "● │", "│╱", "●"]);
    }

    #[test]
    fn spine_takes_merges_from_the_right() {
        // The head 0 merges 2 (first parent) and 1; 1 and 2 fork from 3.
        let parents: &[&[usize]] = &[&[2, 1], &[3], &[3], &[]];
        assert_eq!(render(parents, &[0, 2, 3]), ["●", "│╲", "│ ●", "● │", "│╱", "●"]);
    }

    #[test]
    fn spine_can_follow_a_second_parent() {
        // 0 merges 1 (first parent) and 2; the spine goes 0 -> 2 -> 3, so 1 branches off.
        let parents: &[&[usize]] = &[&[1, 2], &[3], &[3], &[]];
        assert_eq!(render(parents, &[0, 2, 3]), ["●", "│╲", "│ ●", "● │", "│╱", "●"]);
        // Without a spine the first parent stays in line.
        assert_eq!(render(parents, &[]), ["●", "│╲", "● │", "│ ●", "│╱", "●"]);
    }

    fn render(parents: &[&[usize]], spine: &[usize]) -> Vec<String> {
        lines(&layout(parents.iter().copied(), spine, &[]), |c| (c.glyph, c.conn))
    }

    /// The glyphs as drawn, heavy ones included.
    fn render_heavy(parents: &[&[usize]], spine: &[usize], heavy: &[bool]) -> Vec<String> {
        lines(&layout(parents.iter().copied(), spine, heavy), Cell::shown)
    }

    #[test]
    fn heavy_lines_from_heavy_rows() {
        // 0 merges 1 and 2, both on 3. Heavy from 1 down (1 and 3): 0's lines stay light.
        let parents: &[&[usize]] = &[&[1, 2], &[3], &[3], &[]];
        let heavy = [false, true, false, true];
        // 2's light line joins the heavy one heading for 3.
        assert_eq!(render_heavy(parents, &[], &heavy), ["●", "│╲", "● │", "┃ ●", "┃╱", "●"]);
        // Heavy from 0: everything is (diagonals are heavy too, though they look the same).
        let heavy = [true; 4];
        assert_eq!(render_heavy(parents, &[], &heavy), ["●", "┃╲", "● ┃", "┃ ●", "┃╱", "●"]);
        let rows = layout(parents.iter().copied(), &[], &heavy);
        assert!(rows[0].below[0][0].conn_heavy && rows[2].below[0][0].conn_heavy);
    }

    #[test]
    fn heavy_and_light_lines_cross() {
        // The criss-cross: light row 1 joins lane 0 (0's heavy line to 2) with a long reach
        // that crosses lane 1 (0's heavy line to 3).
        let parents: &[&[usize]] = &[&[2, 3], &[3, 2], &[4], &[4], &[]];
        let heavy = [true, false, true, true, true];
        let lines = render_heavy(parents, &[], &heavy);
        assert_eq!(lines[2..5], ["┃ ┃ ●", "┃ ┃╱", "┃╱┃"], "light joins and crosses heavy lanes");
        let rows = layout(parents.iter().copied(), &[], &heavy);
        assert!(!rows[1].below[1][0].conn_heavy, "the light reach stays light");
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
                "●", "│╲", "│ ●", "│ ●", "│  ╲", "●   │", "│╲  │", "│ ● │", "│ ● │", "│ ● │", "│  ╲│",
                "●   │", "●   │", "│╲  │", "│ ● │", "│ ● │", "│  ╲│", "●  ╱", "│ ╱", "│╱", "●",
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
        assert_eq!(
            render(parents, &spine),
            ["  ●", "   ╲", "●   │", "│╲  │", "│ ● │", "│╱  │", "●  ╱", "│ ╱", "│╱", "●"]
        );
        // With the head's history marked, the tip starts right of the column it needs.
        let heavy = [false, true, true, true, true];
        assert_eq!(
            render_heavy(parents, &spine, &heavy),
            ["    ●", "●   │", "┃╲  │", "┃ ● │", "┃╱  │", "●  ╱", "┃ ╱", "┃╱", "●"]
        );
    }

    #[test]
    fn head_history_does_not_join_lines_outside_it() {
        // Row 0 is a tip outside the head's history, on 3. The head 1 merges 4 (spine) and 2,
        // and 2 is also on 3. The head's line through 2 keeps its column; the outside line
        // merges into 3 when it gets there.
        let parents: &[&[usize]] = &[&[3], &[4, 2], &[3], &[4], &[]];
        let spine = [1, 4];
        let heavy = [false, true, true, true, true];
        // (3 also joins the spine early, under its row.)
        assert_eq!(
            render_heavy(parents, &spine, &heavy),
            ["    ●", "●   │", "┃╲  │", "┃ ● │", "┃ ┃╱", "┃ ●", "┃╱", "●"]
        );
        // Unmarked, the younger line through 2 joins the older one.
        assert_eq!(render(parents, &spine)[4..6], ["│ ● │", "│  ╲│"]);
    }

    #[test]
    fn long_reaches_are_staircases() {
        // The head 1 merges 3 (spine) and 2; the tip 0 passes by in column 2 and heads for 3
        // too. Its line leaves column 2 on 2's own line, beside the commit, and goes down one
        // step per line, never along `_`.
        let heavy = [false, true, true, true];
        let got = render_heavy(&[&[3], &[3, 2], &[3], &[]], &[1, 3], &heavy);
        assert_eq!(got, ["    ●", "●   │", "┃╲  │", "┃ ●╱", "┃╱╱", "┃╱", "●"]);
        // Three columns: more connector lines.
        let heavy = [false, true, true, true, true];
        let got = render_heavy(&[&[4], &[4, 2], &[4], &[4], &[]], &[1, 4], &heavy);
        assert_eq!(got[5..], ["┃ ●  ╱", "┃╱  ╱", "┃  ╱", "┃ ╱", "┃╱", "●"]);
    }

    #[test]
    fn run_over_freed_lane() {
        // Lane 2's long reach into 4 passes column 1, freed when 2 joined lane 0.
        let got = render(&[&[3], &[2, 4], &[3], &[4], &[]], &[]);
        assert_eq!(got, ["●", "│ ●", "│ │╲", "│ ● │", "│╱  │", "●  ╱", "│ ╱", "│╱", "●"]);
    }
}
