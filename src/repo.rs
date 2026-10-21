//! Reads the commit history and ref decorations of a repository through libgit2.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::order::{self, Order};

#[derive(Clone)]
pub struct Commit {
    pub oid: Oid,
    /// Row indices of the parents that are part of the history.
    pub parents: Vec<usize>,
    pub author: String,
    /// Author date in the author's own time zone, as `%as` prints it.
    pub date: String,
    pub summary: String,
    /// Committer date, seconds since the epoch.
    pub time: i64,
    /// It has more than one parent in the repository (whether or not all are shown).
    pub merge: bool,
    /// With first parents only: how many commits this merge brought in (reachable from its
    /// other parents, not from its first); 0 otherwise.
    pub merged: usize,
    /// Not reachable from any ref or HEAD: only from a reflog, or from nothing at all.
    pub deleted: bool,
}

impl Commit {
    pub fn short_id(&self) -> String {
        self.oid.to_string()[..7].to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Head,
    Local,
    Remote,
    Tag,
    Other,
}

#[derive(Clone)]
pub struct Decoration {
    #[allow(dead_code)] // for colouring refs by kind later
    pub kind: RefKind,
    pub name: String,
    /// The ref it shows, by full name: the branch for `HEAD -> main`, none for a detached HEAD.
    pub full: Option<String>,
}

/// Which commits that no ref reaches any more are shown as well.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Deleted {
    #[default]
    Hide,
    /// Commits still recorded in some reflog, as `git log --reflog` adds them.
    Reflog,
    /// Reflog commits plus every other commit object in the repository (dangling ones).
    All,
}

impl Deleted {
    pub fn name(self) -> &'static str {
        match self {
            Deleted::Hide => "hide",
            Deleted::Reflog => "reflog",
            Deleted::All => "all",
        }
    }

    pub fn next(self) -> Deleted {
        match self {
            Deleted::Hide => Deleted::Reflog,
            Deleted::Reflog => Deleted::All,
            Deleted::All => Deleted::Hide,
        }
    }
}

/// What to load: the order of the rows, which deleted commits to include, and which refs to
/// leave out together with the commits only they reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub order: Order,
    pub deleted: Deleted,
    /// Ref patterns: `*` and `?` globs, matched against the full name when they start with
    /// `refs/` (or are `HEAD`), against the short name (`main`, `origin/x`, `v1`) otherwise.
    pub hide: Vec<String>,
    /// Whether `hide` applies; off shows everything without forgetting the patterns.
    pub hiding: bool,
    /// Show only commits with a ref label, joined to their nearest such ancestors.
    pub refs_only: bool,
    /// The commit laid out as the head (its first-parent line on the left), instead of HEAD.
    pub head: Option<Oid>,
    /// Follow only first parents, as `git log --first-parent`.
    pub first_parent: bool,
}

impl Default for View {
    fn default() -> Self {
        View {
            order: Order::default(),
            deleted: Deleted::default(),
            hide: Vec::new(),
            hiding: true,
            refs_only: false,
            head: None,
            first_parent: false,
        }
    }
}

impl View {
    pub fn hides(&self, full: &str) -> bool {
        self.hiding && self.hide.iter().any(|p| ref_matches(p, full))
    }
}

fn ref_matches(pattern: &str, full: &str) -> bool {
    let name = if pattern.starts_with("refs/") || pattern == "HEAD" { full } else { short_ref(full) };
    glob(pattern.as_bytes(), name.as_bytes())
}

/// A ref's name without `refs/heads/`, `refs/remotes/` or `refs/tags/`.
pub fn short_ref(full: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .iter()
        .find_map(|p| full.strip_prefix(p))
        .unwrap_or(full)
}

/// Matches `*` (any run of characters, `/` included) and `?` (any one character).
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and how much of the text it has taken so far.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            star = Some((sp, st + 1));
            p = sp + 1;
            t = st + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// The refs and HEAD, as (name, target). Any change to them can change the history shown.
pub type Fingerprint = Vec<(String, String)>;

#[derive(Default)]
pub struct History {
    /// Commits in display order: every commit comes before its parents.
    pub commits: Vec<Commit>,
    /// Refs pointing at each row, in the order `git log --decorate` lists them.
    pub decorations: HashMap<usize, Vec<Decoration>>,
    /// The state of the refs this history was loaded from.
    pub fingerprint: Fingerprint,
    /// A rebase was stopped (on a conflict, say) and is waiting to be continued or aborted.
    pub rebasing: bool,
    /// The row HEAD points at.
    pub head: Option<usize>,
}

/// Loads what `git log --all --exclude='refs/prefetch/*'` would show, in the requested order,
/// plus `--reflog` (or every commit object) when deleted commits are wanted.
pub fn load(path: &Path, view: &View) -> Result<History> {
    let repo = open(path)?;
    // Taken first, so that a change made while loading shows up as a difference later.
    let fingerprint = fingerprint(&repo)?;

    // Starting points in the order git takes them for --all: refs by name, then HEAD.
    let mut refs: Vec<(String, Oid)> = Vec::new();
    for reference in repo.references()? {
        let reference = reference?;
        let name = String::from_utf8_lossy(reference.name_bytes()).into_owned();
        if name.starts_with("refs/prefetch/") || view.hides(&name) {
            continue;
        }
        // Tags of trees or blobs have no commit to show.
        let Ok(commit) = reference.peel_to_commit() else { continue };
        refs.push((name, commit.id()));
    }
    refs.sort();
    let head = if view.hides("HEAD") {
        None
    } else {
        repo.head().ok().and_then(|h| h.peel_to_commit().ok()).map(|c| c.id())
    };
    let mut live_starts: Vec<Oid> = refs.iter().map(|(_, o)| *o).collect();
    live_starts.extend(head);
    let extra_starts = deleted_starts(&repo, &refs, head.is_some(), view.deleted)?;

    let mut walk = repo.revwalk()?;
    if view.first_parent {
        walk.simplify_first_parent()?;
    }
    for &oid in &live_starts {
        walk.push(oid)?;
    }
    for &oid in &extra_starts {
        // A reflog can name a commit that has since been pruned.
        let _ = walk.push(oid);
    }
    let oids = walk.collect::<Result<Vec<Oid>, _>>()?;
    let index: HashMap<Oid, usize> = oids.iter().enumerate().map(|(i, &o)| (o, i)).collect();

    let mut parents: Vec<Vec<usize>> = Vec::with_capacity(oids.len());
    let mut time: Vec<i64> = Vec::with_capacity(oids.len());
    let mut commits: Vec<Commit> = Vec::with_capacity(oids.len());
    for &oid in &oids {
        let commit = repo.find_commit(oid)?;
        let mut ps: Vec<usize> = Vec::new();
        for p in commit.parent_ids() {
            if let Some(&i) = index.get(&p)
                && !ps.contains(&i)
            {
                ps.push(i);
            }
        }
        // With first parents only, the others neither order nor draw anything. (git does the
        // same when it has a commit-graph file; without one it still orders by all parents.)
        if view.first_parent {
            ps.retain(|&i| commit.parent_ids().next() == Some(oids[i]));
        }
        parents.push(ps);
        time.push(commit.time().seconds());
        let author = commit.author();
        let when = author.when();
        commits.push(Commit {
            oid,
            parents: Vec::new(),
            author: String::from_utf8_lossy(author.name_bytes()).into_owned(),
            date: civil_date(when.seconds() + i64::from(when.offset_minutes()) * 60),
            summary: commit.summary_bytes().map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default(),
            time: commit.time().seconds(),
            merge: commit.parent_count() > 1,
            merged: if view.first_parent { merged_count(&repo, &commit) } else { 0 },
            deleted: true,
        });
    }

    // Everything the refs reach is live; the rest came in only through the extra starts.
    let mut todo: Vec<usize> = live_starts.iter().filter_map(|o| index.get(o).copied()).collect();
    while let Some(i) = todo.pop() {
        if std::mem::replace(&mut commits[i].deleted, false) {
            todo.extend(&parents[i]);
        }
    }

    let starts: Vec<usize> =
        live_starts.iter().chain(&extra_starts).filter_map(|o| index.get(o).copied()).collect();
    // The chosen head if it is part of this history, else HEAD.
    let layout_head = [view.head, head].into_iter().flatten().find_map(|h| index.get(&h).copied());
    let rows = order::sort(&parents, &time, &starts, view.order, layout_head);
    let mut row_of = vec![0usize; rows.len()];
    for (row, &i) in rows.iter().enumerate() {
        row_of[i] = row;
    }
    let mut slots: Vec<Option<Commit>> = commits.into_iter().map(Some).collect();
    let commits: Vec<Commit> = rows
        .iter()
        .map(|&i| {
            let mut c = slots[i].take().unwrap();
            c.parents = parents[i].iter().map(|&p| row_of[p]).collect();
            c
        })
        .collect();
    let index: HashMap<Oid, usize> = commits.iter().enumerate().map(|(r, c)| (c.oid, r)).collect();

    let decorated = refs
        .into_iter()
        .filter(|(name, _)| {
            ["refs/heads/", "refs/remotes/", "refs/tags/"].iter().any(|p| name.starts_with(p))
                || name == "refs/stash"
        })
        .collect();
    let mut history = History {
        decorations: decorations(&repo, decorated, head, &index),
        head: head.and_then(|h| index.get(&h).copied()),
        commits,
        fingerprint,
        rebasing: crate::ops::rebase_in_progress(path),
    };
    if view.refs_only {
        let keep = view.head.and_then(|h| index.get(&h).copied());
        history = simplify(&history, |r| history.decorations.contains_key(&r) || keep == Some(r));
    }
    Ok(history)
}

impl History {
    /// A row's text as the list shows it after the graph (`logpretty`'s format); search and
    /// filter match against it.
    pub fn line(&self, row: usize) -> String {
        let c = &self.commits[row];
        let refs = match self.decorations.get(&row) {
            Some(d) => format!(" ({})", d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")),
            None => String::new(),
        };
        format!("{} - ({}) {} -{}({})", c.short_id(), c.date, c.summary, refs, c.author)
    }
}

/// Keeps only the rows `keep` accepts. Each kept commit's parents become the nearest kept
/// commits found along all of its parent paths, first parent's first, without those reached
/// past hidden commits that another of them already reaches (real parents always stay). The rows keep their order, which stays topological:
/// every new parent is an ancestor.
pub fn simplify(history: &History, keep: impl Fn(usize) -> bool) -> History {
    let commits = &history.commits;
    let n = commits.len();
    let kept: Vec<bool> = (0..n).map(keep).collect();

    // For every row, the nearest kept commits at or behind it. Parents have higher row
    // numbers than their children, so going upwards from the bottom sees parents first.
    let mut nearest: Vec<Vec<usize>> = vec![Vec::new(); n];
    for r in (0..n).rev() {
        nearest[r] = if kept[r] { vec![r] } else { reach(&commits[r].parents, &nearest) };
    }
    let mut parents: Vec<Vec<usize>> = (0..n)
        .map(|r| if kept[r] { reach(&commits[r].parents, &nearest) } else { Vec::new() })
        .collect();

    // Drop a parent reached only past hidden commits when another parent already reaches it
    // through kept ones. A real parent that is kept always stays.
    for r in 0..n {
        if parents[r].len() > 1 {
            let ps = &parents[r];
            let real = &commits[r].parents;
            let needed: Vec<usize> = ps
                .iter()
                .copied()
                .filter(|&p| real.contains(&p) || !ps.iter().any(|&q| q != p && reaches(&parents, q, p)))
                .collect();
            parents[r] = needed;
        }
    }

    let mut new_row = vec![usize::MAX; n];
    for (i, r) in (0..n).filter(|&r| kept[r]).enumerate() {
        new_row[r] = i;
    }
    let decorations = history
        .decorations
        .iter()
        .filter(|&(&r, _)| kept[r])
        .map(|(&r, d)| (new_row[r], d.clone()))
        .collect();
    let commits = (0..n)
        .filter(|&r| kept[r])
        .map(|r| {
            let mut c = commits[r].clone();
            c.parents = parents[r].iter().map(|&p| new_row[p]).collect();
            c
        })
        .collect();
    History {
        commits,
        decorations,
        fingerprint: history.fingerprint.clone(),
        rebasing: history.rebasing,
        head: history.head.filter(|&h| kept[h]).map(|h| new_row[h]),
    }
}

/// The union of `nearest` over the given rows, in order and without repeats.
fn reach(rows: &[usize], nearest: &[Vec<usize>]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for &p in rows {
        for &k in &nearest[p] {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// Whether `to` can be reached from `from` along `parents`. Parents have higher row numbers,
/// so rows past `to` need not be searched.
fn reaches(parents: &[Vec<usize>], from: usize, to: usize) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut todo = vec![from];
    while let Some(r) = todo.pop() {
        if r == to {
            return true;
        }
        if r < to && seen.insert(r) {
            todo.extend(&parents[r]);
        }
    }
    false
}

/// How many commits a merge brought in: reachable from its other parents, not its first.
fn merged_count(repo: &Repository, commit: &git2::Commit) -> usize {
    let (Ok(first), true) = (commit.parent_id(0), commit.parent_count() > 1) else { return 0 };
    let Ok(mut walk) = repo.revwalk() else { return 0 };
    for p in commit.parent_ids().skip(1) {
        if walk.push(p).is_err() {
            return 0;
        }
    }
    if walk.hide(first).is_err() {
        return 0;
    }
    walk.count()
}

fn open(path: &Path) -> Result<Repository> {
    Repository::discover(path).with_context(|| format!("not a git repository: {}", path.display()))
}

/// Reads the current state of the refs, for comparing with a loaded history's.
pub fn current_fingerprint(path: &Path) -> Result<Fingerprint> {
    fingerprint(&open(path)?)
}

fn fingerprint(repo: &Repository) -> Result<Fingerprint> {
    let mut out = Fingerprint::new();
    let head = repo.find_reference("HEAD")?;
    let target = |r: &git2::Reference| {
        r.symbolic_target_bytes()
            .map(|t| String::from_utf8_lossy(t).into_owned())
            .or_else(|| r.target().map(|o| o.to_string()))
            .unwrap_or_default()
    };
    out.push(("HEAD".to_string(), target(&head)));
    for reference in repo.references()? {
        let reference = reference?;
        let name = String::from_utf8_lossy(reference.name_bytes()).into_owned();
        if !name.starts_with("refs/prefetch/") {
            let t = target(&reference);
            out.push((name, t));
        }
    }
    out.sort();
    Ok(out)
}

/// Extra starting points for the commits no ref reaches.
/// Hidden refs are not in `refs`, so their reflogs are skipped too; `head` is false when HEAD
/// is hidden.
fn deleted_starts(
    repo: &Repository,
    refs: &[(String, Oid)],
    head: bool,
    deleted: Deleted,
) -> Result<Vec<Oid>> {
    let mut out = Vec::new();
    if deleted == Deleted::Hide {
        return Ok(out);
    }
    let head = head.then_some("HEAD");
    for name in refs.iter().map(|(n, _)| n.as_str()).chain(head) {
        let Ok(log) = repo.reflog(name) else { continue };
        for entry in log.iter() {
            out.extend([entry.id_old(), entry.id_new()].into_iter().filter(|o| !o.is_zero()));
        }
    }
    if deleted == Deleted::All {
        let odb = repo.odb()?;
        let mut all = Vec::new();
        odb.foreach(|oid| {
            all.push(*oid);
            true
        })?;
        all.sort();
        all.dedup();
        out.extend(
            all.into_iter()
                .filter(|&o| odb.read_header(o).is_ok_and(|(_, t)| t == git2::ObjectType::Commit)),
        );
    }
    Ok(out)
}

/// Groups the refs by commit. Git lists them in reverse name order, with HEAD first and
/// merged with the branch it points at (`HEAD -> main`).
fn decorations(
    repo: &Repository,
    mut refs: Vec<(String, Oid)>,
    head: Option<Oid>,
    index: &HashMap<Oid, usize>,
) -> HashMap<usize, Vec<Decoration>> {
    refs.sort();
    let head_branch = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|h| h.symbolic_target().ok().flatten().map(str::to_owned));

    let mut out: HashMap<usize, Vec<Decoration>> = HashMap::new();
    if let Some(head) = head
        && let Some(&row) = index.get(&head)
    {
        let (name, full) = match &head_branch {
            Some(b) if refs.iter().any(|(n, o)| n == b && *o == head) => {
                (format!("HEAD -> {}", short_name(b).1), Some(b.clone()))
            }
            _ => ("HEAD".to_string(), None),
        };
        out.entry(row).or_default().push(Decoration { kind: RefKind::Head, name, full });
    }
    for (name, oid) in refs.iter().rev() {
        let Some(&row) = index.get(oid) else { continue };
        if Some(*oid) == head && head_branch.as_deref() == Some(name.as_str()) {
            continue;
        }
        let full = Some(name.clone());
        let (kind, name) = short_name(name);
        out.entry(row).or_default().push(Decoration { kind, name, full });
    }
    out
}

fn short_name(full: &str) -> (RefKind, String) {
    if let Some(s) = full.strip_prefix("refs/heads/") {
        (RefKind::Local, s.to_string())
    } else if let Some(s) = full.strip_prefix("refs/remotes/") {
        (RefKind::Remote, s.to_string())
    } else if let Some(s) = full.strip_prefix("refs/tags/") {
        (RefKind::Tag, format!("tag: {s}"))
    } else {
        (RefKind::Other, full.to_string())
    }
}

/// Formats seconds since the epoch (already shifted to local time) as YYYY-MM-DD.
fn civil_date(secs: i64) -> String {
    // Howard Hinnant's days_from_civil, inverted.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .envs(env.iter().copied())
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        git_env(dir, args, &[])
    }

    fn commit(dir: &Path, msg: &str, date: &str) {
        let env = [("GIT_AUTHOR_DATE", date), ("GIT_COMMITTER_DATE", date)];
        git_env(dir, &["commit", "-q", "--allow-empty", "-m", msg], &env);
    }

    #[test]
    fn matches_git_log() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        commit(d, "A", "2024-01-01T10:00:00+00:00");
        commit(d, "B\nwith a second line\n\nand a body", "2024-01-02T23:30:00-05:00");
        git(d, &["branch", "feat"]);
        commit(d, "C", "2024-01-03T01:00:00+02:00");
        git(d, &["checkout", "-q", "feat"]);
        commit(d, "D", "2024-01-04T10:00:00+00:00");
        git(d, &["tag", "light"]);
        commit(d, "E", "2024-01-05T10:00:00+00:00");
        git(d, &["checkout", "-q", "main"]);
        git(d, &["merge", "-q", "--no-edit", "feat"]);
        git(d, &["tag", "-a", "-m", "annotated", "v1"]);
        commit(d, "F", "2024-01-07T10:00:00+00:00");
        git(d, &["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
        git(d, &["update-ref", "refs/prefetch/remotes/origin/x", "HEAD~2"]);
        git(d, &["update-ref", "refs/custom/hidden", "HEAD~2"]);
        std::fs::write(d.join("f"), "x").unwrap();
        git(d, &["add", "f"]);
        git(d, &["stash", "-q"]);
        commit(d, "G", "2024-01-08T10:00:00+00:00");
        // Commits only a reflog remembers: an amended one and a deleted branch's.
        git_env(d, &["commit", "-q", "--amend", "--allow-empty", "-m", "G2"], &[("GIT_COMMITTER_DATE", "2024-01-09T10:00:00+00:00")]);
        git(d, &["checkout", "-q", "-b", "tmp"]);
        commit(d, "T", "2024-01-10T10:00:00+00:00");
        git(d, &["checkout", "-q", "main"]);
        git(d, &["branch", "-q", "-D", "tmp"]);
        // A commit nothing records at all.
        let dangling = git(d, &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "dangling"]);
        git(d, &["checkout", "-q", "--detach", "HEAD~1"]);

        let h = load(d, &View::default()).unwrap();
        assert!(h.commits.iter().all(|c| !c.deleted));

        let ids = |h: &History| h.commits.iter().map(|c| c.oid.to_string()).collect::<Vec<_>>();
        let git_ids = |args: &[&str]| {
            let mut a = vec!["log", "--all", "--exclude=refs/prefetch/*", "--format=%H"];
            a.extend(args);
            git(d, &a).lines().map(str::to_owned).collect::<Vec<_>>()
        };
        let git_topo = load(d, &View { order: Order::GitTopo, ..View::default() }).unwrap();
        assert_eq!(ids(&git_topo), git_ids(&["--topo-order"]));
        let by_date = load(d, &View { order: Order::Date, ..View::default() }).unwrap();
        assert_eq!(ids(&by_date), git_ids(&["--date-order"]));

        // git orders --first-parent by first parents only when it has a commit-graph.
        git(d, &["commit-graph", "write", "--reachable"]);
        let first = load(d, &View { order: Order::GitTopo, first_parent: true, ..View::default() }).unwrap();
        assert_eq!(ids(&first), git_ids(&["--first-parent", "--topo-order"]));
        assert!(first.commits.iter().all(|c| c.parents.len() <= 1));
        assert!(first.commits.iter().any(|c| c.merge), "the merge is still known as one");

        let reflog = load(d, &View { order: Order::GitTopo, deleted: Deleted::Reflog, ..View::default() }).unwrap();
        assert_eq!(ids(&reflog), git_ids(&["--reflog", "--topo-order"]));
        let live = git_ids(&[]);
        for c in &reflog.commits {
            assert_eq!(c.deleted, !live.contains(&c.oid.to_string()), "{}", c.summary);
        }
        assert!(reflog.commits.iter().any(|c| c.deleted && c.summary == "T"));
        assert!(reflog.commits.iter().any(|c| c.deleted && c.summary == "G"));
        assert!(!ids(&reflog).contains(&dangling.trim().to_string()));

        let all = load(d, &View { deleted: Deleted::All, ..View::default() }).unwrap();
        assert!(ids(&all).contains(&dangling.trim().to_string()));
        assert_eq!(all.commits.len(), reflog.commits.len() + 1);

        let ours: std::collections::BTreeSet<String> =
            h.commits.iter().map(|c| c.oid.to_string()).collect();
        let theirs: std::collections::BTreeSet<String> =
            git(d, &["rev-list", "--all", "--exclude=refs/prefetch/*"]).lines().map(str::to_owned).collect();
        assert_eq!(ours, theirs);

        for (i, c) in h.commits.iter().enumerate() {
            assert!(c.parents.iter().all(|&p| p > i), "{} shown after a parent", c.short_id());
        }

        let log = git(d, &["log", "--all", "--exclude=refs/prefetch/*", "--format=%H%x09%D%x09%as%x09%s"]);
        for line in log.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let row = h.commits.iter().position(|c| c.oid.to_string() == f[0]).unwrap();
            let decs = h.decorations.get(&row).map(|d| {
                d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
            });
            assert_eq!(decs.unwrap_or_default(), f[1], "decorations of {}", f[3]);
            assert_eq!(h.commits[row].date, f[2], "date of {}", f[3]);
            assert_eq!(h.commits[row].summary, f[3]);
        }
    }

    #[test]
    fn hiding_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        commit(d, "A", "2024-01-01T10:00:00+00:00");
        commit(d, "B", "2024-01-02T10:00:00+00:00");
        git(d, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        commit(d, "S1", "2024-01-03T10:00:00+00:00");
        git(d, &["tag", "t"]);
        commit(d, "S2", "2024-01-04T10:00:00+00:00");
        git(d, &["checkout", "-q", "main"]);

        let shown = |hide: &[&str]| {
            let view = View { hide: hide.iter().map(|s| s.to_string()).collect(), ..View::default() };
            let h = load(d, &view).unwrap();
            let decs: Vec<String> =
                h.decorations.values().flatten().map(|d| d.name.clone()).collect();
            (h.commits.iter().map(|c| c.summary.clone()).collect::<Vec<_>>(), decs)
        };
        // The same commits as git shows, in any order (ours starts with HEAD).
        let git_shown = |exclude: &[&str]| {
            let mut args = vec!["log".to_string(), "--format=%s".to_string()];
            args.extend(exclude.iter().map(|e| format!("--exclude={e}")));
            args.push("--all".into());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let mut v: Vec<String> = git(d, &args).lines().map(str::to_owned).collect();
            v.sort();
            v
        };
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };

        assert_eq!(shown(&[]).0, ["B", "S2", "S1", "A"]);
        // The tag keeps S1; only S2 goes with the branch.
        let (commits, decs) = shown(&["side"]);
        assert_eq!(commits, ["B", "S1", "A"]);
        assert_eq!(sorted(commits), git_shown(&["refs/heads/side"]));
        assert!(!decs.contains(&"side".to_string()));
        assert_eq!(shown(&["side", "t"]).0, ["B", "A"]);
        assert_eq!(shown(&["refs/heads/s*", "refs/tags/?"]).0, ["B", "A"]);
        // Hiding main leaves HEAD, which still reaches B, now without its branch name.
        let (commits, decs) = shown(&["main"]);
        assert_eq!(commits, ["B", "S2", "S1", "A"]);
        assert!(decs.contains(&"HEAD".to_string()));
        assert_eq!(shown(&["main", "HEAD"]).0, ["S2", "S1", "A"]);
        assert_eq!(sorted(shown(&["main", "HEAD"]).0), git_shown(&["refs/heads/main", "HEAD"]));

        let mut off = View { hide: vec!["side".into(), "t".into()], ..View::default() };
        off.hiding = false;
        assert_eq!(load(d, &off).unwrap().commits.len(), 4);
    }

    /// Commits given top to bottom as (name, parent names); `labelled` get a tag.
    fn synthetic(rows: &[(&str, &[&str])], labelled: &[&str]) -> History {
        let row = |name: &str| rows.iter().position(|(n, _)| *n == name).unwrap();
        let commits = rows
            .iter()
            .enumerate()
            .map(|(i, (name, ps))| Commit {
                oid: Oid::from_bytes(&[i as u8 + 1; 20]).unwrap(),
                parents: ps.iter().map(|p| row(p)).collect(),
                author: String::new(),
                date: String::new(),
                summary: name.to_string(),
                time: 0,
                merge: false,
                merged: 0,
                deleted: false,
            })
            .collect();
        let decorations = labelled
            .iter()
            .map(|&n| (row(n), vec![Decoration { kind: RefKind::Tag, name: n.into(), full: None }]))
            .collect();
        History { commits, decorations, fingerprint: vec![], rebasing: false, head: None }
    }

    /// Each row as "name: parents".
    fn shape(h: &History) -> Vec<String> {
        h.commits
            .iter()
            .map(|c| {
                let ps: Vec<&str> = c.parents.iter().map(|&p| h.commits[p].summary.as_str()).collect();
                format!("{}: {}", c.summary, ps.join(" "))
            })
            .collect()
    }

    fn only_refs(h: History, keep: Option<usize>) -> History {
        simplify(&h, |r| h.decorations.contains_key(&r) || keep == Some(r))
    }

    #[test]
    fn line_is_the_row_text() {
        let h = synthetic(&[("a", &[])], &["a"]);
        assert_eq!(h.line(0), "0101010 - () a - (a)()");
    }

    #[test]
    fn only_refs_joins_kept_commits() {
        // a-b-c-d-e-f, newest (f) on top; b, d and e have no labels: a-c-f is left.
        let line = [("f", &["e"][..]), ("e", &["d"]), ("d", &["c"]), ("c", &["b"]), ("b", &["a"]), ("a", &[])];
        assert_eq!(shape(&only_refs(synthetic(&line, &["a", "c", "f"]), None)), ["f: c", "c: a", "a: "]);

        // m merges the labelled side branch s into the main line; the main line's unlabelled
        // commit leads to base, which s reaches too, so that edge adds nothing and goes.
        let merge = [("m", &["x", "s"][..]), ("s", &["base"]), ("x", &["base"]), ("base", &[])];
        assert_eq!(shape(&only_refs(synthetic(&merge, &["m", "s", "base"]), None)), ["m: s", "s: base", "base: "]);

        // Two labelled tips forking from an unlabelled commit both lead to the labelled root.
        let fork = [("p", &["fork"][..]), ("q", &["fork"]), ("fork", &["root"]), ("root", &[])];
        assert_eq!(shape(&only_refs(synthetic(&fork, &["p", "q", "root"]), None)), ["p: root", "q: root", "root: "]);

        // A labelled merge of two labelled parents keeps both, the first parent first.
        let both = [("m", &["x", "y"][..]), ("x", &["r"]), ("y", &["r"]), ("r", &[])];
        assert_eq!(shape(&only_refs(synthetic(&both, &["m", "x", "y"]), None)), ["m: x y", "x: ", "y: "]);
    }

    #[test]
    fn simplify_keeps_real_parents() {
        // m merges x (first parent) and the branch b, which forked from x. Keeping everything,
        // m still has both parents, although b reaches x.
        let rows = [("m", &["x", "b"][..]), ("b", &["x"]), ("x", &[])];
        let h = synthetic(&rows, &[]);
        assert_eq!(shape(&simplify(&h, |_| true)), ["m: x b", "b: x", "x: "]);
    }

    #[test]
    fn only_refs_keeps_the_layout_head() {
        let line = [("f", &["e"][..]), ("e", &["d"]), ("d", &["c"]), ("c", &[])];
        assert_eq!(shape(&only_refs(synthetic(&line, &["f", "c"]), Some(2))), ["f: d", "d: c", "c: "]);
    }

    #[test]
    fn globs() {
        assert!(glob(b"origin/*", b"origin/a/b"));
        assert!(glob(b"v?.*", b"v6.17"));
        assert!(glob(b"*", b""));
        assert!(!glob(b"main", b"mainline"));
        assert!(!glob(b"a*b", b"ac"));
        assert!(glob(b"a*b*c", b"axxbyyc"));
        assert!(ref_matches("origin/x", "refs/remotes/origin/x"));
        assert!(ref_matches("v1", "refs/tags/v1"));
        assert!(!ref_matches("refs/heads/v1", "refs/tags/v1"));
    }

    #[test]
    fn dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(-1), "1969-12-31");
    }
}

/// `GITNAV_COMPARE_REPO=<path> cargo test -- --ignored` checks the order against git there.
#[cfg(test)]
#[test]
#[ignore]
fn order_matches_git_in_real_repo() {
    let path = std::env::var("GITNAV_COMPARE_REPO").expect("GITNAV_COMPARE_REPO is set");
    let path = Path::new(&path);
    // Topo and date order start with HEAD, so only git-topo is git's order exactly.
    for (first_parent, extra) in [(false, None), (true, Some("--first-parent"))] {
        let h = load(path, &View { order: Order::GitTopo, first_parent, ..View::default() }).unwrap();
        let mut args = vec!["log", "--all", "--exclude=refs/prefetch/*", "--topo-order", "--format=%H"];
        args.extend(extra);
        let out = std::process::Command::new("git").args(&args).current_dir(path).output().unwrap();
        let theirs: Vec<String> = String::from_utf8(out.stdout).unwrap().lines().map(str::to_owned).collect();
        let ours: Vec<String> = h.commits.iter().map(|c| c.oid.to_string()).collect();
        let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
        assert_eq!(first, None, "{extra:?}: first difference at row {first:?}");
        assert_eq!(ours.len(), theirs.len(), "{extra:?}");
    }
}

/// `GITNAV_COMPARE_REPO=<path> cargo test -- --ignored --nocapture order_metrics` prints how
/// often each order breaks the parent chain and how wide the graph gets.
#[cfg(test)]
#[test]
#[ignore]
fn order_metrics() {
    let path = std::env::var("GITNAV_COMPARE_REPO").expect("GITNAV_COMPARE_REPO is set");
    for order in [Order::GitTopo, Order::Topo, Order::Date] {
        let h = load(Path::new(&path), &View { order, ..View::default() }).unwrap();
        for (i, c) in h.commits.iter().enumerate() {
            assert!(c.parents.iter().all(|&p| p > i));
        }
        let breaks = (1..h.commits.len()).filter(|&i| !h.commits[i - 1].parents.contains(&i)).count();
        let rows = crate::graph::layout(h.commits.iter().map(|c| c.parents.as_slice()), &[], &[]);
        let lanes: usize = rows.iter().map(|r| r.cells.iter().filter(|c| c.glyph != ' ').count()).sum();
        let widest = rows.iter().flat_map(|r| std::iter::once(&r.cells).chain(&r.below)).map(Vec::len).max().unwrap_or(0);
        println!(
            "{:>8}: {} rows, next row not a parent {} times, lanes per row {:.2}, widest {}",
            order.name(), h.commits.len(), breaks, lanes as f64 / rows.len() as f64, widest
        );
    }
}
