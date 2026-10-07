//! Vertical commit order. `GitTopo` and `Date` are git's `--topo-order` and `--date-order`, as
//! `revision.c` implements them; `Topo` is git's topo order changed to stay with the parents.
//!
//! All of them show no parent before all of its children. They start from the commits nobody
//! points at (the tips), in date order, and differ in which ready commit comes next: git's topo
//! order takes the one made ready most recently (a stack), so it follows one line of history
//! as far as it can; date order takes the newest by committer date.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Order {
    /// Lines of history kept together, and a blocked line's parent shown as soon as possible.
    #[default]
    Topo,
    /// Newest commit first, as `git log --date-order`.
    Date,
    /// Exactly `git log --topo-order`, which `git log --graph` uses.
    GitTopo,
}

impl Order {
    pub fn name(self) -> &'static str {
        match self {
            Order::Topo => "topo",
            Order::Date => "date",
            Order::GitTopo => "git-topo",
        }
    }

    pub fn next(self) -> Order {
        match self {
            Order::Topo => Order::Date,
            Order::Date => Order::GitTopo,
            Order::GitTopo => Order::Topo,
        }
    }
}

/// Returns commit indices in display order.
///
/// `parents` are in git's parent order and refer only to commits in the set. `starts` are the
/// starting points in the order git would be given them (`--all`: refs by name, then HEAD).
///
/// With a `head`, topo and date order begin with it: first the commits that lead to it (they
/// must come before it), then the head, then everything else, the head's own line first in
/// topo order. Git's topo order ignores `head`.
pub fn sort(
    parents: &[Vec<usize>],
    time: &[i64],
    starts: &[usize],
    order: Order,
    head: Option<usize>,
) -> Vec<usize> {
    let n = parents.len();
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (c, ps) in parents.iter().enumerate() {
        for &p in ps {
            kids[p].push(c);
        }
    }
    let pending: Vec<usize> = kids.iter().map(Vec::len).collect();

    // A stable sort, like git's commit_list_sort_by_date.
    let mut tips: Vec<usize> = starts.iter().copied().filter(|&s| pending[s] == 0).collect();
    let mut seen = vec![false; n];
    tips.retain(|&t| !std::mem::replace(&mut seen[t], true));
    tips.sort_by_key(|&t| Reverse(time[t]));

    let mut walk = Walk {
        parents,
        time,
        kids,
        pending,
        shown: vec![false; n],
        visited: vec![0; n],
        search: 0,
        out: Vec::with_capacity(n),
    };
    let Some(head) = head.filter(|_| order != Order::GitTopo) else {
        walk.run(order, tips, None);
        return walk.out;
    };

    // The head and everything that reaches it. A child of such a commit reaches it too, so
    // this set can be shown completely before anything else.
    let mut first = vec![false; n];
    let mut todo = vec![head];
    while let Some(c) = todo.pop() {
        if !std::mem::replace(&mut first[c], true) {
            todo.extend(&walk.kids[c]);
        }
    }
    let (lead, rest): (Vec<usize>, Vec<usize>) = tips.into_iter().partition(|&t| first[t]);
    let deferred = walk.run(order, lead, Some(&first));
    // What became ready last (the head's parents) goes first.
    walk.run(order, deferred.into_iter().rev().chain(rest).collect(), None);
    walk.out
}

/// How many waiting commits one search for a blocking branch may look at before giving up.
const SEARCH_LIMIT: usize = 10_000;

struct Walk<'a> {
    parents: &'a [Vec<usize>],
    time: &'a [i64],
    kids: Vec<Vec<usize>>,
    /// Children of each commit not shown yet; a commit is ready at zero.
    pending: Vec<usize>,
    shown: Vec<bool>,
    /// Marks for the blocking-branch search, by search number.
    visited: Vec<u32>,
    search: u32,
    out: Vec<usize>,
}

impl Walk<'_> {
    /// Shows commits starting from `initial` (in the order they should come) until none is
    /// ready. With a `scope`, only commits in it are shown; the others that become ready are
    /// returned, in the order they became ready.
    fn run(&mut self, order: Order, initial: Vec<usize>, scope: Option<&[bool]>) -> Vec<usize> {
        let in_scope = |c: usize| scope.is_none_or(|s| s[c]);
        let mut deferred = Vec::new();
        match order {
            Order::Date => {
                // Newest first; among equal dates, the one queued first.
                let mut queue = BinaryHeap::new();
                let mut seq = 0usize;
                for c in initial {
                    queue.push((self.time[c], Reverse(seq), c));
                    seq += 1;
                }
                while let Some((_, _, c)) = queue.pop() {
                    self.out.push(c);
                    for &p in &self.parents[c] {
                        self.pending[p] -= 1;
                        if self.pending[p] == 0 {
                            if in_scope(p) {
                                queue.push((self.time[p], Reverse(seq), p));
                                seq += 1;
                            } else {
                                deferred.push(p);
                            }
                        }
                    }
                }
            }
            Order::Topo | Order::GitTopo => {
                // A commit can sit on the stack twice when a search pulled it to the top.
                let mut stack: Vec<usize> = initial.into_iter().rev().collect();
                while let Some(c) = stack.pop() {
                    if std::mem::replace(&mut self.shown[c], true) {
                        continue;
                    }
                    self.out.push(c);
                    let mut advanced = false;
                    for &p in &self.parents[c] {
                        self.pending[p] -= 1;
                        if self.pending[p] == 0 {
                            if in_scope(p) {
                                stack.push(p);
                                advanced = true;
                            } else {
                                deferred.push(p);
                            }
                        }
                    }
                    if order == Order::Topo
                        && !advanced
                        && let Some(&blocked) = self.parents[c].first()
                        && self.pending[blocked] > 0
                        && let Some(r) = self.blocking_branch(blocked)
                        && in_scope(r)
                    {
                        stack.push(r);
                    }
                }
            }
        }
        deferred
    }

    /// Topo order stays with the current commit's parents. Like git's, it follows a line of
    /// history while it can (a stack of ready commits). When the line is blocked, because its
    /// next commit still has children to show, git continues with whatever is on top of the
    /// stack, often an unrelated branch tip. This finds the branch that blocks it instead: the
    /// nearest ready commit from which an unshown child of the blocked commit can be reached,
    /// newest first among equals, so that the blocked commit comes as soon as possible.
    fn blocking_branch(&mut self, blocked: usize) -> Option<usize> {
        self.search += 1;
        let mut level: Vec<usize> = Vec::new();
        for &k in &self.kids[blocked] {
            if !self.shown[k] && self.visited[k] != self.search {
                self.visited[k] = self.search;
                level.push(k);
            }
        }
        let mut budget = SEARCH_LIMIT;
        while !level.is_empty() && budget > 0 {
            if let Some(&r) =
                level.iter().filter(|&&u| self.pending[u] == 0).max_by_key(|&&u| self.time[u])
            {
                return Some(r);
            }
            budget = budget.saturating_sub(level.len());
            let mut next = Vec::new();
            for &u in &level {
                for &k in &self.kids[u] {
                    if !self.shown[k] && self.visited[k] != self.search {
                        self.visited[k] = self.search;
                        next.push(k);
                    }
                }
            }
            level = next;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from `git help log`: commits 1..=8 numbered in timestamp order.
    ///
    /// ```text
    /// ---1----2----4----7
    ///     \              \
    ///      3----5----6----8---
    /// ```
    fn man_page_example(order: Order) -> Vec<usize> {
        // Index i is commit i + 1; 8 merges 6 (first parent) and 7.
        let parents = vec![vec![], vec![0], vec![0], vec![1], vec![2], vec![4], vec![3], vec![5, 6]];
        let time: Vec<i64> = (1..=8).collect();
        sort(&parents, &time, &[7], order, None).into_iter().map(|i| i + 1).collect()
    }

    #[test]
    fn date_order() {
        assert_eq!(man_page_example(Order::Date), [8, 7, 6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn topo_order() {
        // One of the two orders the man page allows; the stack pops the last parent first.
        assert_eq!(man_page_example(Order::GitTopo), [8, 7, 4, 2, 6, 5, 3, 1]);
        // Blocked at 1 after 2, the search finds 6 ready, which leads to 1 through 5 and 3.
        assert_eq!(man_page_example(Order::Topo), [8, 7, 4, 2, 6, 5, 3, 1]);
    }

    #[test]
    fn blocked_line_continues_with_the_blocking_branch() {
        // Tips 0 (newest), 2 and 3. 0 and 3 share the parent 1; 2 is unrelated.
        let parents = vec![vec![1], vec![], vec![], vec![1]];
        let time = vec![10, 1, 9, 8];
        // git goes on with the next tip by date, 2, while 1 waits for 3.
        assert_eq!(sort(&parents, &time, &[0, 2, 3], Order::GitTopo, None), [0, 2, 3, 1]);
        // Topo shows 3 next, which lets 1 follow right away.
        assert_eq!(sort(&parents, &time, &[0, 2, 3], Order::Topo, None), [0, 3, 1, 2]);
    }

    #[test]
    fn tips_newest_first() {
        // Two unrelated tips; the newer one is shown first whatever order they are given in.
        let parents = vec![vec![], vec![0], vec![], vec![2]];
        let time = vec![1, 2, 3, 4];
        assert_eq!(sort(&parents, &time, &[1, 3], Order::Topo, None), [3, 2, 1, 0]);
        assert_eq!(sort(&parents, &time, &[1, 3], Order::GitTopo, None), [3, 2, 1, 0]);
        assert_eq!(sort(&parents, &time, &[1, 3, 1], Order::Date, None), [3, 2, 1, 0]);
    }

    #[test]
    fn head_comes_first() {
        // 0 is the newest tip (on 1); the head 2 is an older tip on 3.
        let parents = vec![vec![1], vec![], vec![3], vec![]];
        let time = vec![9, 2, 5, 1];
        assert_eq!(sort(&parents, &time, &[0, 2], Order::Topo, None), [0, 1, 2, 3]);
        // The head first, then its line, then the rest.
        assert_eq!(sort(&parents, &time, &[0, 2], Order::Topo, Some(2)), [2, 3, 0, 1]);
        // Date order: the head first, then by date.
        assert_eq!(sort(&parents, &time, &[0, 2], Order::Date, Some(2)), [2, 0, 1, 3]);
        // Git's topo order stays as git has it.
        assert_eq!(sort(&parents, &time, &[0, 2], Order::GitTopo, Some(2)), [0, 1, 2, 3]);
    }

    #[test]
    fn what_leads_to_the_head_comes_before_it() {
        // A chain 0 -> 1 -> 2 with the head at 1, and a newer unrelated tip 3 on 4.
        let parents = vec![vec![1], vec![2], vec![], vec![4], vec![]];
        let time = vec![5, 4, 3, 9, 1];
        assert_eq!(sort(&parents, &time, &[0, 3], Order::Topo, Some(1)), [0, 1, 2, 3, 4]);
        // A merge reaching the head through its second parent brings its first parent's line
        // (not leading to the head) only after the head.
        let parents = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let time = vec![9, 8, 2, 1];
        assert_eq!(sort(&parents, &time, &[0], Order::Topo, Some(2)), [0, 2, 1, 3]);
    }
}
