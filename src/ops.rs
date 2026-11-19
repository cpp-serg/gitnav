//! Operations that run the git CLI: pull and rebase (libgit2 has no pull, no rebase with
//! autostash, and does not use git's credential helpers or ssh setup), `git show`, `git diff`
//! (also into diffnav or difftastic), and deleting branches and tags, locally and on remotes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use git2::{Repository, RepositoryState};

use crate::repo::{is_worktree, short};

pub enum PullOutcome {
    /// The branch moved (or was already up to date): the summary says which.
    Pulled(String),
    /// The pull failed for another reason than a conflict; git's output says why.
    Failed,
    /// The rebase stopped on a conflict and is still in progress.
    Conflict(Vec<String>),
    /// The pull went through, but the stashed local changes conflict with it.
    StashConflict(Vec<String>),
}

/// The working tree of the repository at `path`.
pub fn workdir(path: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(path)?;
    match repo.workdir() {
        Some(w) => Ok(w.to_path_buf()),
        None => bail!("a bare repository has no working tree to pull into"),
    }
}

pub fn rebase_in_progress(path: &Path) -> bool {
    Repository::discover(path).is_ok_and(|r| {
        matches!(
            r.state(),
            RepositoryState::Rebase
                | RepositoryState::RebaseInteractive
                | RepositoryState::RebaseMerge
                | RepositoryState::ApplyMailboxOrRebase
        )
    })
}

/// Paths with unresolved conflicts in the index.
pub fn conflicted_files(path: &Path) -> Vec<String> {
    let Ok(repo) = Repository::discover(path) else { return Vec::new() };
    let Ok(index) = repo.index() else { return Vec::new() };
    let Ok(conflicts) = index.conflicts() else { return Vec::new() };
    let mut files: Vec<String> = conflicts
        .filter_map(Result::ok)
        .filter_map(|c| c.our.or(c.their).or(c.ancestor))
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    files.dedup();
    files
}

/// `git pull --rebase --autostash`, with the terminal handed to git: its progress, messages
/// and any password or passphrase prompt go straight to the user.
pub fn pull(path: &Path) -> Result<PullOutcome> {
    let dir = workdir(path)?;
    let before = head(&dir);
    let status = Command::new("git")
        .args(["pull", "--rebase", "--autostash"])
        .current_dir(&dir)
        .status()
        .context("running git pull")?;

    if rebase_in_progress(&dir) {
        return Ok(PullOutcome::Conflict(conflicted_files(&dir)));
    }
    if !status.success() {
        return Ok(PullOutcome::Failed);
    }
    let conflicts = conflicted_files(&dir);
    if !conflicts.is_empty() {
        return Ok(PullOutcome::StashConflict(conflicts));
    }
    let after = head(&dir);
    Ok(PullOutcome::Pulled(match (before, after) {
        (Some(b), Some(a)) if b != a => format!("pulled {:.7}..{:.7}", b.to_string(), a.to_string()),
        _ => "already up to date".to_string(),
    }))
}

/// What the commit view or a diff tool shows: one commit with its change (`git show`), or the
/// diff between two commits (`git diff from to`). Either commit can be the working tree
/// (`repo::worktree_id`): its own change is the diff from HEAD, `git diff HEAD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Commit(git2::Oid),
    Between(git2::Oid, git2::Oid),
}

impl Change {
    /// The working tree's own change as the diff from HEAD to it; anything else as it is.
    pub fn resolve(self, path: &Path) -> Result<Change> {
        Ok(match self {
            Change::Commit(c) if is_worktree(c) => {
                let head = Repository::discover(path)?.head()?.peel_to_commit()?.id();
                Change::Between(head, c)
            }
            change => change,
        })
    }

    /// The git command and its arguments; `options` go after the command name. The working
    /// tree is no revision: `git diff from` diffs from a commit to it, `-R` the other way.
    fn args(self, options: &[&str]) -> Vec<String> {
        let (command, revs): (&str, Vec<String>) = match self {
            Change::Commit(c) => ("show", vec![c.to_string()]),
            Change::Between(from, to) if is_worktree(to) => ("diff", vec![from.to_string()]),
            Change::Between(from, to) if is_worktree(from) => ("diff", vec!["-R".into(), to.to_string()]),
            Change::Between(from, to) => ("diff", vec![from.to_string(), to.to_string()]),
        };
        let mut args = vec![command.to_string()];
        args.extend(options.iter().map(|o| o.to_string()));
        args.extend(revs);
        args
    }

    /// A title such as `git show 1a2b3c4` or `difft 1a2b3c4 <WTREE>`.
    pub fn title(self, tool: DiffTool) -> String {
        let revs = match self {
            Change::Commit(c) => short(c),
            Change::Between(from, to) => format!("{} {}", short(from), short(to)),
        };
        match (tool, self) {
            (DiffTool::Git, Change::Commit(_)) => format!("git show {revs}"),
            _ => format!("{} {revs}", tool.name()),
        }
    }
}

/// Whether `change` changes no file: two commits with the same tree, or a commit with the
/// same tree as its first parent (an empty tree for a root commit). A change to or from the
/// working tree counts as one: its row is only there while it differs from HEAD.
pub fn changes_nothing(path: &Path, change: Change) -> Result<bool> {
    let repo = Repository::discover(path)?;
    let tree = |oid| -> Result<git2::Oid> { Ok(repo.find_commit(oid)?.tree_id()) };
    Ok(match change {
        Change::Commit(c) | Change::Between(c, _) | Change::Between(_, c) if is_worktree(c) => false,
        Change::Between(from, to) => tree(from)? == tree(to)?,
        Change::Commit(c) => {
            let commit = repo.find_commit(c)?;
            let before = match commit.parent_ids().next() {
                Some(p) => tree(p)?,
                None => repo.treebuilder(None)?.write()?,
            };
            commit.tree_id() == before
        }
    })
}

/// The two selected commits in the order to diff them: from the ancestor to its descendant
/// if one is an ancestor of the other, else from the second selected to the first. The working
/// tree descends from HEAD and everything HEAD reaches.
pub fn diff_order(path: &Path, first: git2::Oid, second: git2::Oid) -> Result<(git2::Oid, git2::Oid)> {
    let repo = Repository::discover(path)?;
    let head = || -> Result<git2::Oid> { Ok(repo.head()?.peel_to_commit()?.id()) };
    // Whether `older` is an ancestor of `newer`.
    let ancestor = |older: git2::Oid, newer: git2::Oid| -> Result<bool> {
        if is_worktree(older) {
            return Ok(false);
        }
        let newer = if is_worktree(newer) { head()? } else { newer };
        Ok(older == newer || repo.graph_descendant_of(newer, older)?)
    };
    Ok(if first != second && ancestor(first, second)? { (first, second) } else { (second, first) })
}

/// `change` as coloured text for the commit view, `width` wide: through the pager git would
/// use for it (delta, say), or with difftastic as git's external diff. A merge commit gets
/// `merge_text`.
pub fn text(path: &Path, change: Change, tool: DiffTool, width: usize) -> Result<String> {
    let dir = workdir_or_gitdir(path)?;
    if let Change::Commit(c) = change {
        let parents = parents(path, c)?;
        if parents.len() > 1 {
            return merge_text(&dir, c, &parents, tool, width);
        }
    }
    render(&dir, change.args(&[]), tool, width)
}

/// The git command `args` (`args[0]` names it) as coloured text, `width` wide: through the
/// pager git would use for that command, or with difftastic as git's external diff.
fn render(dir: &Path, mut args: Vec<String>, tool: DiffTool, width: usize) -> Result<String> {
    if tool != DiffTool::Difft {
        let pager = configured_pager(dir, &args[0]).and_then(|p| filter_pager(&p));
        return run_through(dir, &args, width, pager.as_deref());
    }
    args.splice(1..1, ["--ext-diff".to_string(), "--color=always".to_string()]);
    let out = Command::new("git")
        .args(["-c", "diff.external=difft", "--no-pager"])
        .args(&args)
        .env("DFT_COLOR", "always")
        .env("DFT_WIDTH", width.to_string())
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("running git with difft")?;
    if !out.status.success() {
        bail!("git with difft failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A commit's parents, first parent first.
fn parents(path: &Path, oid: git2::Oid) -> Result<Vec<git2::Oid>> {
    Ok(Repository::discover(path)?.find_commit(oid)?.parent_ids().collect())
}

/// Commits listed in the merge view at most.
const MERGED_LISTED: usize = 100;

/// A merge for the commit view: its message, the commits it brought in, what was done by
/// hand beyond the automatic merge (`--remerge-diff`, only when there is something), and its
/// changes against the first parent: the branch's whole change, as a pull request shows it.
/// git's own view of a merge, the combined diff, is empty for most merges.
fn merge_text(dir: &Path, merge: git2::Oid, parents: &[git2::Oid], tool: DiffTool, width: usize) -> Result<String> {
    let title = |text: String| format!("\n\x1b[1;33m{text}\x1b[0m\n\n");
    let mut out = render(dir, vec!["show".into(), "--no-patch".into(), merge.to_string()], tool, width)?;

    let mut range: Vec<String> = vec![format!("^{}", parents[0])];
    range.extend(parents[1..].iter().map(|p| p.to_string()));
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git").arg("--no-pager").args(args).args(&range).current_dir(dir).stdin(Stdio::null()).output()?;
        if !out.status.success() {
            bail!("git {} failed: {}", args[0], String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let count: usize = git(&["rev-list", "--count"])?.trim().parse().unwrap_or(0);
    let from: Vec<String> = parents[1..].iter().map(|&p| short(p)).collect();
    out += &title(format!("Brought in from {} ({count} commit{}):", from.join(", "), if count == 1 { "" } else { "s" }));
    out += &git(&["log", "--oneline", "--color=always", &format!("--max-count={MERGED_LISTED}")])?;
    if count > MERGED_LISTED {
        out += &format!("… and {} more\n", count - MERGED_LISTED);
    }

    if parents.len() == 2 {
        let resolution = resolution_text(dir, merge, tool, width)?;
        if !resolution.is_empty() {
            out += &title("Done by hand, against the automatic merge of the parents (git show --remerge-diff):".into());
            out += &resolution;
        }
    }
    out += &title(format!("Changes against the first parent, {}:", short(parents[0])));
    out += &render(dir, vec!["diff".into(), parents[0].to_string(), merge.to_string()], tool, width)?;
    Ok(out)
}

/// What a merge has beyond the automatic merge of its parents (`git show --remerge-diff`):
/// conflict resolutions and changes of its own. Empty if nothing.
fn resolution_text(dir: &Path, merge: git2::Oid, tool: DiffTool, width: usize) -> Result<String> {
    let text = render(dir, vec!["show".into(), "--remerge-diff".into(), "--format=".into(), merge.to_string()], tool, width)?;
    Ok(if crate::ansi::is_blank(&text) { String::new() } else { text })
}

/// `m`: a merge's message and what it has beyond the automatic merge, or `None` if nothing.
pub fn resolution(path: &Path, merge: git2::Oid, tool: DiffTool, width: usize) -> Result<Option<String>> {
    let dir = workdir_or_gitdir(path)?;
    if parents(path, merge)?.len() != 2 {
        bail!("{} is not a merge of two parents", short(merge));
    }
    let resolution = resolution_text(&dir, merge, tool, width)?;
    if resolution.is_empty() {
        return Ok(None);
    }
    let header = render(&dir, vec!["show".into(), "--no-patch".into(), merge.to_string()], tool, width)?;
    Ok(Some(format!("{header}\n{resolution}")))
}

/// `change` piped into diffnav, which has the terminal until it quits: run it with the terminal
/// handed over. A merge shows against its first parent.
pub fn diffnav(path: &Path, change: Change) -> Result<()> {
    let dir = workdir_or_gitdir(path)?;
    // Plain text: diffnav reads the diff itself, and colour codes would get in its way. It
    // reads only the diff, so a commit comes without its message.
    let options: &[&str] = match change {
        Change::Commit(_) => &["--no-color", "--no-ext-diff", "--format=", "--diff-merges=first-parent"],
        Change::Between(..) => &["--no-color", "--no-ext-diff"],
    };
    let mut git = Command::new("git")
        .arg("--no-pager")
        .args(change.args(options))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    let status = Command::new("diffnav")
        .current_dir(&dir)
        .stdin(git.stdout.take().expect("piped"))
        .status()
        .context("running diffnav")?;
    let out = git.wait_with_output().context("running git")?;
    if !out.status.success() {
        bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if !status.success() {
        bail!("diffnav failed ({status})");
    }
    Ok(())
}

/// What Enter and `d` show a change with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffTool {
    /// diffnav, a diff viewer with a file tree; it takes over the terminal.
    #[default]
    Diffnav,
    /// Difftastic, a structural diff, as git's external diff; shown in the commit view.
    Difft,
    /// `git show` or `git diff` through the configured pager, shown in the commit view.
    Git,
}

impl DiffTool {
    pub fn name(self) -> &'static str {
        match self {
            DiffTool::Diffnav => "diffnav",
            DiffTool::Difft => "difft",
            DiffTool::Git => "git diff",
        }
    }

    pub fn next(self) -> DiffTool {
        match self {
            DiffTool::Diffnav => DiffTool::Difft,
            DiffTool::Difft => DiffTool::Git,
            DiffTool::Git => DiffTool::Diffnav,
        }
    }

    /// The program it needs besides git.
    fn program(self) -> Option<&'static str> {
        match self {
            DiffTool::Diffnav => Some("diffnav"),
            DiffTool::Difft => Some("difft"),
            DiffTool::Git => None,
        }
    }

    /// Whether its program is on `PATH`.
    pub fn available(self) -> bool {
        self.program().is_none_or(|p| on_path(p, std::env::var_os("PATH")))
    }
}

/// Whether `program` is an executable file in one of the directories of `path`.
fn on_path(program: &str, path: Option<std::ffi::OsString>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = path else { return false };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(program)).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The git command `args` in `dir`, piped through `pager` if there is one.
fn run_through(dir: &Path, args: &[String], width: usize, pager: Option<&str>) -> Result<String> {
    let name = format!("git {}", args[0]);
    let mut git = Command::new("git");
    git.args(["--no-pager", &args[0], "--color=always"]).args(&args[1..]).current_dir(dir);
    let Some(pager) = pager else {
        let out = git.stdin(Stdio::null()).output().with_context(|| format!("running {name}"))?;
        if !out.status.success() {
            bail!("{name} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    };

    let mut show = git
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {name}"))?;
    // Run the way git runs its pager: through the shell, with git's defaults for less and lv.
    let out = Command::new("sh")
        .args(["-c", pager])
        .current_dir(dir)
        .env("LESS", std::env::var("LESS").unwrap_or_else(|_| "FRX".into()))
        .env("LV", "-c")
        .env("COLUMNS", width.to_string())
        .env("GIT_CONFIG_PARAMETERS", delta_config(width, std::env::var("GIT_CONFIG_PARAMETERS").ok()))
        .stdin(show.stdout.take().expect("piped"))
        .output()
        .with_context(|| format!("running the pager: {pager}"))?;
    let shown = show.wait_with_output().with_context(|| format!("running {name}"))?;
    if !shown.status.success() {
        bail!("{name} failed: {}", String::from_utf8_lossy(&shown.stderr).trim());
    }
    if !out.status.success() {
        bail!("the pager ({pager}) failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pager git would use for `git <command>` in `dir`: `pager.<command>` if it names one
/// (none if it is false), else `git var GIT_PAGER` ($GIT_PAGER, core.pager, $PAGER, git's
/// default).
fn configured_pager(dir: &Path, command: &str) -> Option<String> {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    match git(&["config", "--type=bool-or-str", "--get", &format!("pager.{command}")]).as_deref() {
        Some("false") => None,
        Some("true") | None => git(&["var", "GIT_PAGER"]),
        Some(own) => Some(own.to_string()),
    }
}

/// `pager` as the command to filter the text through, if it is one that rewrites the text
/// (delta, diff-so-fancy, …) rather than one that pages it interactively (less, more), whose
/// job the commit view does itself.
fn filter_pager(pager: &str) -> Option<String> {
    let program = pager.split_whitespace().next()?;
    match Path::new(program).file_name()?.to_str()? {
        "less" | "more" | "most" | "lv" | "cat" => None,
        _ => Some(pager.to_string()),
    }
}

/// `GIT_CONFIG_PARAMETERS` for the pager: `existing`, plus delta's width and no paging of its
/// own. Delta reads its settings from git's config, this variable included, so they reach it
/// however the pager command starts it (`exec delta || exec less`, say). Not `COLUMNS` or a
/// `--width` added to the command: delta before 0.20 ignores `COLUMNS`, and a flag can only be
/// added where the command is just delta.
fn delta_config(width: usize, existing: Option<String>) -> String {
    let ours = format!("'delta.width'='{width}' 'delta.paging'='never'");
    match existing.filter(|e| !e.trim().is_empty()) {
        Some(e) => format!("{e} {ours}"),
        None => ours,
    }
}

/// Where to run git for reading: the working tree, or the git directory of a bare repository.
fn workdir_or_gitdir(path: &Path) -> Result<PathBuf> {
    let repo = Repository::discover(path)?;
    Ok(repo.workdir().unwrap_or(repo.path()).to_path_buf())
}

/// `git rebase --abort`: puts the branch back as it was before the pull, and with it the
/// changes the pull stashed. Returns git's output when it fails.
pub fn abort_rebase(path: &Path) -> Result<()> {
    let out = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(workdir(path)?)
        .stdin(Stdio::null())
        .output()
        .context("running git rebase --abort")?;
    if !out.status.success() {
        bail!("git rebase --abort failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// A named ref that can be deleted, here or on a remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deletion {
    LocalBranch(String),
    /// A linked worktree, removed with everything in it (`git worktree remove --force`).
    Worktree {
        /// As `repo::Worktree` names it, and where it is.
        name: String,
        path: PathBuf,
        /// The branch checked out there, if any: it can only be deleted with the worktree.
        branch: Option<String>,
        /// Tracked files with changes not committed, and untracked files, there: lost with it.
        changed: usize,
        untracked: usize,
    },
    /// The branch on the remote (and with it the remote-tracking ref `remote/branch`).
    RemoteBranch { remote: String, branch: String },
    LocalTag(String),
    RemoteTag { remote: String, tag: String },
}

impl Deletion {
    /// What to call it in the dialog: kind and name.
    pub fn label(&self) -> (&'static str, String) {
        match self {
            Deletion::LocalBranch(b) => ("branch", b.clone()),
            Deletion::Worktree { name, .. } => ("WT", name.clone()),
            Deletion::RemoteBranch { remote, branch } => ("remote branch", format!("{remote}/{branch}")),
            Deletion::LocalTag(t) => ("tag", t.clone()),
            Deletion::RemoteTag { remote, tag } => ("remote tag", format!("{tag} on {remote}")),
        }
    }

    /// Whether it removes a worktree whose tracked files have changes not committed.
    pub fn loses_changes(&self) -> bool {
        matches!(self, Deletion::Worktree { changed, .. } if *changed > 0)
    }

    pub fn is_worktree(&self) -> bool {
        matches!(self, Deletion::Worktree { .. })
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Deletion::RemoteBranch { .. } | Deletion::RemoteTag { .. })
    }

    /// The name it sorts by, so that a branch or tag and its remote copies stay together.
    fn name(&self) -> &str {
        match self {
            Deletion::LocalBranch(n) | Deletion::LocalTag(n) => n,
            // Next to the branch checked out there.
            Deletion::Worktree { branch: Some(n), .. } | Deletion::Worktree { name: n, .. } => n,
            Deletion::RemoteBranch { branch: n, .. } | Deletion::RemoteTag { tag: n, .. } => n,
        }
    }
}

/// What can be deleted at a commit: each with a warning, if any; plus notes on what cannot be
/// (the checked-out branch) or could not be checked (a remote that did not answer).
pub struct Deletables {
    pub entries: Vec<(Deletion, Option<String>)>,
    pub notes: Vec<String>,
}

/// The branches and tags pointing at `oid`, their remote-tracking branches, and the same tags
/// on each remote (asked with `git ls-remote`, which must not prompt: see `batch`).
pub fn deletables(path: &Path, oid: git2::Oid) -> Result<Deletables> {
    let repo = Repository::discover(path)?;
    let dir = workdir_or_gitdir(path)?;
    let head = repo.head().ok().and_then(|h| h.target());
    let current = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|h| h.symbolic_target_bytes().map(|t| String::from_utf8_lossy(t).into_owned()));
    let mut entries = Vec::new();
    let mut notes = Vec::new();
    let mut tags = Vec::new();
    let worktrees = crate::repo::worktrees(&repo);
    // remote/HEAD names each remote's default branch.
    let defaults: Vec<String> = repo
        .references_glob("refs/remotes/*/HEAD")?
        .flatten()
        .filter_map(|r| r.symbolic_target_bytes().map(|t| String::from_utf8_lossy(t).into_owned()))
        .collect();
    for reference in repo.references()? {
        let reference = reference?;
        let name = String::from_utf8_lossy(reference.name_bytes()).into_owned();
        if reference.peel_to_commit().map(|c| c.id()).ok() != Some(oid) {
            continue;
        }
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            if current.as_deref() == Some(name.as_str()) {
                notes.push(format!("{branch} is checked out; switch to another branch to delete it"));
                continue;
            }
            let merged = head.is_some_and(|h| h == oid || repo.graph_descendant_of(h, oid).unwrap_or(false));
            let mut warnings: Vec<String> = Vec::new();
            if !merged {
                warnings.push("not merged into HEAD".into());
            }
            // Checked out in another worktree: it goes only with that worktree's entry.
            match worktrees.iter().find(|w| !w.current && w.branch.as_deref() == Some(name.as_str())) {
                Some(w) if w.main => {
                    notes.push(format!("{branch} is checked out in the main worktree {}, which git does not remove", w.name));
                    continue;
                }
                Some(w) => warnings.insert(0, format!("checked out in WT: {}, which goes too", w.name)),
                None => {}
            }
            entries.push((Deletion::LocalBranch(branch.to_string()), (!warnings.is_empty()).then(|| warnings.join("; "))));
        } else if let Some(rest) = name.strip_prefix("refs/remotes/") {
            // remote/HEAD only says which branch is the remote's default.
            if reference.symbolic_target_bytes().is_some() {
                continue;
            }
            let Some((remote, branch)) = split_remote(&repo, rest) else { continue };
            let warning = defaults.contains(&name).then(|| format!("the default branch on {remote}"));
            entries.push((Deletion::RemoteBranch { remote, branch }, warning));
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            entries.push((Deletion::LocalTag(tag.to_string()), None));
            tags.push(tag.to_string());
        }
    }
    // The linked worktrees here; not the main one, which git does not remove, nor this one.
    for w in worktrees.iter().filter(|w| w.head == oid && !w.main && !w.current) {
        let (changed, untracked) = match crate::repo::worktree_state(&w.path) {
            Ok(state) => state,
            Err(e) => {
                notes.push(format!("WT: {} could not be checked: {e:#}", w.name));
                continue;
            }
        };
        let branch = w.branch.as_deref().and_then(|b| b.strip_prefix("refs/heads/")).map(str::to_owned);
        let mut warnings = Vec::new();
        if changed > 0 {
            warnings.push(format!("UNCOMMITTED CHANGES IN {changed} TRACKED FILE{}", if changed == 1 { "" } else { "S" }));
        }
        if untracked > 0 {
            warnings.push(format!("{untracked} untracked file{}", if untracked == 1 { "" } else { "s" }));
        }
        if let Some(b) = &branch {
            warnings.push(format!("branch {b} is checked out here"));
        }
        let what = Deletion::Worktree { name: w.name.clone(), path: w.path.clone(), branch, changed, untracked };
        entries.push((what, (!warnings.is_empty()).then(|| warnings.join("; "))));
    }
    if !tags.is_empty() {
        for remote in remote_names(&repo) {
            match remote_tags(&dir, &remote, &tags) {
                Ok(found) => entries.extend(
                    found.into_iter().map(|tag| (Deletion::RemoteTag { remote: remote.clone(), tag }, None)),
                ),
                Err(e) => notes.push(format!("could not check {remote} for these tags: {e:#}")),
            }
        }
    }
    let key = |d: &Deletion| (d.name().to_string(), d.is_remote(), d.is_worktree(), d.label());
    entries.sort_by_key(|(d, _)| key(d));
    Ok(Deletables { entries, notes })
}

/// Splits `origin/feature/x` into the remote and the branch, by the remotes configured
/// (a remote's name may itself contain a slash).
fn split_remote(repo: &Repository, rest: &str) -> Option<(String, String)> {
    let remote = remote_names(repo)
        .into_iter()
        .filter(|r| rest.starts_with(&format!("{r}/")))
        .max_by_key(String::len)?;
    let branch = rest[remote.len() + 1..].to_string();
    Some((remote, branch))
}

fn remote_names(repo: &Repository) -> Vec<String> {
    let Ok(remotes) = repo.remotes() else { return Vec::new() };
    remotes.iter().filter_map(|r| r.ok().flatten().map(str::to_owned)).collect()
}

/// Which of `tags` the remote has.
fn remote_tags(dir: &Path, remote: &str, tags: &[String]) -> Result<Vec<String>> {
    let mut cmd = batch(dir);
    cmd.args(["ls-remote", "--tags", remote]).args(tags.iter().map(|t| format!("refs/tags/{t}")));
    let out = cmd.output().context("running git ls-remote")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let listed = String::from_utf8_lossy(&out.stdout);
    Ok(tags
        .iter()
        .filter(|t| listed.lines().any(|l| l.split('\t').nth(1) == Some(&format!("refs/tags/{t}"))))
        .cloned()
        .collect())
}

/// git for a remote operation that must not stop to ask anything (it runs while the TUI owns
/// the terminal): no credential prompts, and ssh in batch mode unless ssh is set up otherwise.
fn batch(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    let ssh_configured = std::env::var_os("GIT_SSH_COMMAND").is_some()
        || std::env::var_os("GIT_SSH").is_some()
        || Command::new("git")
            .args(["config", "--get", "core.sshCommand"])
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success());
    if !ssh_configured {
        cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=5");
    }
    cmd
}

/// Deletes one ref and says what happened. Remote deletions are pushes, which may ask for a
/// passphrase: run them with the terminal handed to git.
pub fn delete(path: &Path, what: &Deletion) -> Result<String> {
    let dir = workdir_or_gitdir(path)?;
    let run = |args: &[&str]| -> Result<std::process::Output> {
        Command::new("git").args(args).current_dir(&dir).stdin(Stdio::null()).output().context("running git")
    };
    let failed = |out: &std::process::Output| String::from_utf8_lossy(&out.stderr).trim().to_string();
    let (label, name) = what.label();
    let out = match what {
        Deletion::LocalBranch(b) => run(&["branch", "-D", b])?,
        // --force: asked for, and needed anyway with submodules or untracked files there.
        Deletion::Worktree { path, .. } => run(&["worktree", "remove", "--force", &path.to_string_lossy()])?,
        Deletion::LocalTag(t) => run(&["tag", "-d", t])?,
        Deletion::RemoteTag { remote, tag } => run(&["push", remote, "--delete", &format!("refs/tags/{tag}")])?,
        Deletion::RemoteBranch { remote, branch } => {
            let out = run(&["push", remote, "--delete", branch])?;
            if !out.status.success() && failed(&out).contains("remote ref does not exist") {
                // Already gone there: only the stale remote-tracking ref is left.
                let local = run(&["branch", "-d", "-r", &format!("{remote}/{branch}")])?;
                if !local.status.success() {
                    bail!("deleting {label} {name}: {}", failed(&local));
                }
                return Ok(format!("{remote}/{branch} was already gone on {remote}; removed the stale ref"));
            }
            out
        }
    };
    if !out.status.success() {
        bail!("deleting {label} {name}: {}", failed(&out));
    }
    Ok(format!("deleted {label} {name}"))
}

fn head(dir: &Path) -> Option<git2::Oid> {
    Repository::open(dir).ok()?.head().ok()?.target()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// An upstream with one file, a clone of it, and a second clone that pushes to it.
    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let up = tmp.path().join("up.git");
        let (mine, theirs) = (tmp.path().join("mine"), tmp.path().join("theirs"));
        git(tmp.path(), &["init", "-q", "--bare", "-b", "main", up.to_str().unwrap()]);
        for d in [&mine, &theirs] {
            git(tmp.path(), &["clone", "-q", up.to_str().unwrap(), d.to_str().unwrap()]);
            git(d, &["config", "user.name", "Tester"]);
            git(d, &["config", "user.email", "t@example.com"]);
        }
        std::fs::write(theirs.join("f"), "base\n").unwrap();
        git(&theirs, &["add", "f"]);
        git(&theirs, &["commit", "-q", "-m", "base"]);
        git(&theirs, &["push", "-q", "origin", "main"]);
        git(&mine, &["pull", "-q", "origin", "main"]);
        git(&mine, &["branch", "-q", "--set-upstream-to=origin/main"]);
        (tmp, mine, theirs)
    }

    fn push_change(theirs: &Path, file: &str, text: &str) {
        std::fs::write(theirs.join(file), text).unwrap();
        git(theirs, &["add", file]);
        git(theirs, &["commit", "-q", "-m", file]);
        git(theirs, &["push", "-q", "origin", "main"]);
    }

    // These run git with the user's real config (pull must behave as it would for them), but
    // in throwaway repositories with their own identity.

    #[test]
    fn pull_rebases_and_keeps_local_changes() {
        let (_tmp, mine, theirs) = setup();
        push_change(&theirs, "g", "upstream\n");
        std::fs::write(mine.join("local"), "mine\n").unwrap();
        git(&mine, &["add", "local"]);
        git(&mine, &["commit", "-q", "-m", "local"]);
        std::fs::write(mine.join("f"), "edited, not committed\n").unwrap();

        assert!(matches!(pull(&mine).unwrap(), PullOutcome::Pulled(s) if s.starts_with("pulled ")));
        assert_eq!(std::fs::read_to_string(mine.join("f")).unwrap(), "edited, not committed\n");
        assert_eq!(git(&mine, &["log", "--format=%s"]), "local\ng\nbase\n");
        assert!(matches!(pull(&mine).unwrap(), PullOutcome::Pulled(s) if s == "already up to date"));
    }

    #[test]
    fn conflict_then_abort() {
        let (_tmp, mine, theirs) = setup();
        push_change(&theirs, "f", "theirs\n");
        std::fs::write(mine.join("f"), "mine\n").unwrap();
        git(&mine, &["commit", "-q", "-am", "mine"]);
        std::fs::write(mine.join("other"), "dirty\n").unwrap();
        git(&mine, &["add", "other"]);

        match pull(&mine).unwrap() {
            PullOutcome::Conflict(files) => assert_eq!(files, ["f"]),
            _ => panic!("expected a conflict"),
        }
        assert!(rebase_in_progress(&mine));

        abort_rebase(&mine).unwrap();
        assert!(!rebase_in_progress(&mine));
        assert_eq!(git(&mine, &["log", "--format=%s", "-1"]), "mine\n");
        assert_eq!(std::fs::read_to_string(mine.join("other")).unwrap(), "dirty\n", "stash restored");
    }

    #[test]
    fn show_gives_the_commit_and_its_diff() {
        let (_tmp, mine, _theirs) = setup();
        let head = Repository::open(&mine).unwrap().head().unwrap().target().unwrap();
        // The text as git shows it, without a pager and without the colour codes.
        let text = plain(&run_through(&mine, &["show".into(), head.to_string()], 100, None).unwrap());
        assert!(text.contains(&format!("commit {head}")), "{text}");
        assert!(text.contains("\n+base\n"), "{text}");
    }

    /// Text with git's colour codes taken out.
    fn plain(text: &str) -> String {
        crate::ansi::to_lines(text)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n")
            .collect()
    }

    #[test]
    fn diff_goes_from_the_first_to_the_second() {
        let (_tmp, mine, _theirs) = setup();
        let rev = |r: &str| git2::Oid::from_str(git(&mine, &["rev-parse", r]).trim()).unwrap();
        let old = rev("HEAD");
        std::fs::write(mine.join("added"), "new line\n").unwrap();
        git(&mine, &["add", "added"]);
        git(&mine, &["commit", "-q", "-m", "add"]);
        let new = rev("HEAD");
        let run = |a: git2::Oid, b: git2::Oid| {
            plain(&run_through(&mine, &["diff".into(), a.to_string(), b.to_string()], 100, None).unwrap())
        };
        assert!(run(old, new).contains("\n+new line\n"));
        assert!(run(new, old).contains("\n-new line\n"));
    }

    #[test]
    fn show_through_a_pager() {
        let (_tmp, mine, _theirs) = setup();
        let head = Repository::open(&mine).unwrap().head().unwrap().target().unwrap();
        let args = ["show".to_string(), head.to_string()];
        let text = run_through(&mine, &args, 100, Some("sed s/^/X/")).unwrap();
        assert!(!text.is_empty() && text.lines().all(|l| l.starts_with('X')), "{text}");
        assert!(run_through(&mine, &args, 100, Some("false")).is_err(), "a failing pager is reported");
    }

    #[test]
    fn diff_tools() {
        assert_eq!(DiffTool::default(), DiffTool::Diffnav);
        let cycle = [DiffTool::Diffnav, DiffTool::Difft, DiffTool::Git, DiffTool::Diffnav];
        assert!(cycle.windows(2).all(|w| w[0].next() == w[1]));
        assert!(DiffTool::Git.available(), "git diff needs nothing else");

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("tool");
        std::fs::write(&tool, "").unwrap();
        let path = || Some(std::env::join_paths([tmp.path(), &bin]).unwrap());
        assert!(!on_path("tool", path()), "not executable");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(on_path("tool", path()));
        assert!(!on_path("bin", path()), "a directory is not a program");
        assert!(!on_path("tool", None));
    }

    #[test]
    fn changes_order_and_difft() {
        let (_tmp, mine, _theirs) = setup();
        let rev = |r: &str| git2::Oid::from_str(git(&mine, &["rev-parse", r]).trim()).unwrap();
        let commit = |file: &str, message: &str| {
            std::fs::write(mine.join(file), "new line\n").unwrap();
            git(&mine, &["add", file]);
            git(&mine, &["commit", "-q", "-m", message]);
            rev("HEAD")
        };
        let old = rev("HEAD");
        git(&mine, &["commit", "-q", "--allow-empty", "-m", "empty"]);
        let empty = rev("HEAD");
        let new = commit("added", "add");
        git(&mine, &["checkout", "-q", "-b", "side", &old.to_string()]);
        let side = commit("other", "side");

        assert!(changes_nothing(&mine, Change::Between(old, empty)).unwrap());
        assert!(changes_nothing(&mine, Change::Commit(empty)).unwrap());
        assert!(!changes_nothing(&mine, Change::Between(empty, new)).unwrap());
        assert!(!changes_nothing(&mine, Change::Commit(new)).unwrap());
        assert!(!changes_nothing(&mine, Change::Commit(old)).unwrap(), "a root commit adds its files");

        // An ancestor and its descendant go from the ancestor, in either selection order.
        assert_eq!(diff_order(&mine, old, new).unwrap(), (old, new));
        assert_eq!(diff_order(&mine, new, old).unwrap(), (old, new));
        // Unrelated: from the second selected to the first.
        assert_eq!(diff_order(&mine, side, new).unwrap(), (new, side));
        assert_eq!(diff_order(&mine, new, side).unwrap(), (side, new));

        assert_eq!(Change::Commit(new).title(DiffTool::Git), format!("git show {:.7}", new.to_string()));
        assert_eq!(Change::Between(old, new).title(DiffTool::Difft), format!("difft {:.7} {:.7}", old.to_string(), new.to_string()));
        if DiffTool::Difft.available() {
            let between = plain(&text(&mine, Change::Between(old, new), DiffTool::Difft, 80).unwrap());
            assert!(between.contains("added") && between.contains("new line"), "{between}");
            let shown = plain(&text(&mine, Change::Commit(new), DiffTool::Difft, 80).unwrap());
            assert!(shown.contains(&format!("commit {new}")) && shown.contains("new line"), "{shown}");

            // A merge: against its first parent (side), so it brings in the file added on new.
            git(&mine, &["merge", "-q", "--no-ff", "-m", "merge", &new.to_string()]);
            let merged = plain(&text(&mine, Change::Commit(rev("HEAD")), DiffTool::Difft, 80).unwrap());
            assert!(!merged.contains("diff --cc"), "{merged}");
            assert!(merged.contains("added") && merged.contains("new line"), "{merged}");
        }
    }

    #[test]
    fn worktree_changes() {
        let (_tmp, mine, _theirs) = setup();
        let rev = |r: &str| git2::Oid::from_str(git(&mine, &["rev-parse", r]).trim()).unwrap();
        let old = rev("HEAD");
        std::fs::write(mine.join("added"), "committed\n").unwrap();
        git(&mine, &["add", "added"]);
        git(&mine, &["commit", "-q", "-m", "add"]);
        let head = rev("HEAD");
        git(&mine, &["checkout", "-q", "-b", "side", &old.to_string()]);
        std::fs::write(mine.join("other"), "side\n").unwrap();
        git(&mine, &["add", "other"]);
        git(&mine, &["commit", "-q", "-m", "side"]);
        let side = rev("HEAD");
        git(&mine, &["checkout", "-q", "-"]);
        std::fs::write(mine.join("added"), "changed\n").unwrap();
        let wt = crate::repo::worktree_id();

        // Its own change is the diff from HEAD.
        assert_eq!(Change::Commit(wt).resolve(&mine).unwrap(), Change::Between(head, wt));
        assert_eq!(Change::Commit(head).resolve(&mine).unwrap(), Change::Commit(head));
        assert!(!changes_nothing(&mine, Change::Between(head, wt)).unwrap());
        let diff = |c: Change| plain(&run_through(&mine, &c.args(&[]), 100, None).unwrap());
        let own = diff(Change::Between(head, wt));
        assert!(own.contains("-committed\n+changed"), "{own}");
        assert!(diff(Change::Between(wt, head)).contains("-changed\n+committed"));
        assert_eq!(Change::Between(head, wt).title(DiffTool::Git), format!("git diff {:.7} <WTREE>", head.to_string()));

        // It descends from HEAD and what HEAD reaches; not from a side branch.
        assert_eq!(diff_order(&mine, wt, head).unwrap(), (head, wt));
        assert_eq!(diff_order(&mine, head, wt).unwrap(), (head, wt));
        assert_eq!(diff_order(&mine, wt, old).unwrap(), (old, wt));
        assert_eq!(diff_order(&mine, side, wt).unwrap(), (wt, side), "unrelated: second to first");
        assert_eq!(diff_order(&mine, wt, side).unwrap(), (side, wt));
    }

    #[test]
    fn merge_view_and_resolution() {
        let (_tmp, mine, _theirs) = setup();
        let rev = |r: &str| git2::Oid::from_str(git(&mine, &["rev-parse", r]).trim()).unwrap();
        let change = |file: &str, text: &str, message: &str| {
            std::fs::write(mine.join(file), text).unwrap();
            git(&mine, &["add", file]);
            git(&mine, &["commit", "-q", "-m", message]);
        };
        let base = rev("HEAD").to_string();
        // A forced merge of a branch with two commits.
        git(&mine, &["checkout", "-q", "-b", "feat"]);
        change("a", "a\n", "feat one");
        change("b", "b\n", "feat two");
        git(&mine, &["checkout", "-q", "-b", "forced", &base]);
        git(&mine, &["merge", "-q", "--no-ff", "-m", "forced merge", "feat"]);
        let forced = rev("HEAD");
        // A conflict resolved by hand.
        git(&mine, &["checkout", "-q", "-b", "x", &base]);
        change("f", "x\n", "x");
        git(&mine, &["checkout", "-q", "-b", "y", &base]);
        change("f", "y\n", "y");
        let merged = Command::new("git").args(["merge", "-q", "x"]).current_dir(&mine).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
        assert!(!merged.status.success(), "it conflicts");
        std::fs::write(mine.join("f"), "x and y\n").unwrap();
        git(&mine, &["add", "f"]);
        git(&mine, &["commit", "-q", "--no-edit"]);
        let resolved = rev("HEAD");

        let view = |m| plain(&text(&mine, Change::Commit(m), DiffTool::Git, 100).unwrap());
        let shown = view(forced);
        assert!(shown.contains("forced merge"), "{shown}");
        assert!(shown.contains("Brought in from") && shown.contains("(2 commits):"), "{shown}");
        assert!(shown.contains("feat one") && shown.contains("feat two"), "{shown}");
        assert!(!shown.contains("Done by hand"), "nothing done by hand: {shown}");
        assert!(shown.contains("Changes against the first parent") && shown.contains("+a") && shown.contains("+b"), "{shown}");
        assert!(resolution(&mine, forced, DiffTool::Git, 100).unwrap().is_none());

        let shown = view(resolved);
        assert!(shown.contains("(1 commit):") && shown.contains("Done by hand"), "{shown}");
        assert!(shown.contains("x and y"), "{shown}");
        let only = plain(&resolution(&mine, resolved, DiffTool::Git, 100).unwrap().unwrap());
        assert!(only.contains("x and y") && !only.contains("Changes against"), "{only}");
        assert!(resolution(&mine, rev("x"), DiffTool::Git, 100).is_err(), "not a merge");
    }

    #[test]
    fn branches_in_worktrees() {
        let (tmp, mine, _theirs) = setup();
        let top = std::fs::canonicalize(tmp.path()).unwrap();
        let head = Repository::open(&mine).unwrap().head().unwrap().target().unwrap();
        let (clean, dirty) = (top.join("clean"), top.join("dirty"));
        git(&mine, &["worktree", "add", "-q", clean.to_str().unwrap(), "-b", "c"]);
        git(&mine, &["worktree", "add", "-q", dirty.to_str().unwrap(), "-b", "d"]);
        std::fs::write(dirty.join("f"), "changed\n").unwrap();
        std::fs::write(dirty.join("new"), "x").unwrap();

        let found = deletables(&mine, head).unwrap();
        let labels: Vec<(&str, String)> = found.entries.iter().map(|(d, _)| d.label()).collect();
        let (c, d) = (clean.display().to_string(), dirty.display().to_string());
        // Each branch, then the worktree it is checked out in.
        let remote = ("remote branch", "origin/main".to_string());
        assert_eq!(labels, [("branch", "c".to_string()), ("WT", c.clone()), ("branch", "d".to_string()), ("WT", d.clone()), remote]);
        let warning = |i: usize| found.entries[i].1.clone().unwrap_or_default();
        assert_eq!(warning(0), format!("checked out in WT: {c}, which goes too"));
        assert!(matches!(&found.entries[1].0, Deletion::Worktree { changed: 0, untracked: 0, branch: Some(b), .. } if b == "c"));
        assert!(!found.entries[1].0.loses_changes());
        assert!(matches!(&found.entries[3].0, Deletion::Worktree { changed: 1, untracked: 1, .. }));
        assert!(found.entries[3].0.loses_changes());
        assert_eq!(warning(3), "UNCOMMITTED CHANGES IN 1 TRACKED FILE; 1 untracked file; branch d is checked out here");

        // From a linked worktree: the main one's branch cannot go, nor this one's.
        let from_clean = deletables(&clean, head).unwrap();
        assert!(from_clean.notes.iter().any(|n| n.starts_with("main is checked out in the main worktree")), "{:?}", from_clean.notes);
        assert!(from_clean.notes.iter().any(|n| n.starts_with("c is checked out;")), "its own: {:?}", from_clean.notes);
        assert!(!from_clean.entries.iter().any(|(e, _)| e.label() == ("WT", c.clone())), "not the one gitnav is in");

        // The worktree alone: changes and all; the branch stays.
        assert_eq!(delete(&mine, &found.entries[3].0).unwrap(), format!("deleted WT {d}"));
        assert!(!dirty.exists());
        assert!(!git(&mine, &["worktree", "list"]).contains("dirty"));
        assert_eq!(git(&mine, &["branch", "--list", "d"]).trim(), "d");
        // Then the branch, no longer checked out anywhere.
        delete(&mine, &found.entries[2].0).unwrap();
        assert!(git(&mine, &["branch", "--list", "d"]).is_empty());
    }

    #[test]
    fn which_pagers_filter() {
        assert_eq!(filter_pager("delta").as_deref(), Some("delta"));
        assert_eq!(filter_pager("/usr/local/bin/delta --dark").as_deref(), Some("/usr/local/bin/delta --dark"));
        assert_eq!(filter_pager("diff-so-fancy | less").as_deref(), Some("diff-so-fancy | less"));
        assert_eq!(filter_pager("less -RS"), None);
        assert_eq!(filter_pager("/usr/bin/more"), None);
        assert_eq!(filter_pager("cat"), None);
        assert_eq!(filter_pager(""), None);
    }

    #[test]
    fn the_pager_gets_the_width() {
        assert_eq!(delta_config(90, None), "'delta.width'='90' 'delta.paging'='never'");
        assert_eq!(delta_config(90, Some("'a.b'='c'".into())), "'a.b'='c' 'delta.width'='90' 'delta.paging'='never'");

        let (_tmp, mine, _theirs) = setup();
        let head = Repository::open(&mine).unwrap().head().unwrap().target().unwrap();
        let args = ["show".to_string(), head.to_string()];
        let said = run_through(&mine, &args, 123, Some("cat >/dev/null; printf %s \"$GIT_CONFIG_PARAMETERS\"")).unwrap();
        assert!(said.ends_with("'delta.width'='123' 'delta.paging'='never'"), "{said}");

        // Delta inside a shell construct, as `pager.show` may have it: its rules span the width.
        if on_path("delta", std::env::var_os("PATH")) {
            let construct = "command -v delta >/dev/null && exec delta || exec cat";
            let text = plain(&run_through(&mine, &args, 100, Some(construct)).unwrap());
            let widest = text.lines().map(|l| l.chars().filter(|&c| c == '─').count()).max().unwrap_or(0);
            assert!((95..=100).contains(&widest), "widest rule {widest}:\n{text}");
        }
    }

    #[test]
    fn pager_show_false_means_none() {
        let (_tmp, mine, _theirs) = setup();
        git(&mine, &["config", "pager.show", "false"]);
        assert_eq!(configured_pager(&mine, "show"), None);
        git(&mine, &["config", "pager.show", "tac"]);
        assert_eq!(configured_pager(&mine, "show").as_deref(), Some("tac"));
        git(&mine, &["config", "pager.diff", "rev"]);
        assert_eq!(configured_pager(&mine, "diff").as_deref(), Some("rev"));
    }

    #[test]
    fn delete_branches_and_tags_here_and_on_the_remote() {
        let (tmp, mine, _theirs) = setup();
        let up = tmp.path().join("up.git");
        let head = Repository::open(&mine).unwrap().head().unwrap().target().unwrap();
        // At HEAD: the branch feat (also on origin), the tags v1 (also on origin) and v2.
        git(&mine, &["branch", "feat"]);
        git(&mine, &["push", "-q", "origin", "feat"]);
        git(&mine, &["tag", "v1"]);
        git(&mine, &["push", "-q", "origin", "v1"]);
        git(&mine, &["tag", "v2"]);
        // A branch HEAD does not contain, elsewhere.
        git(&mine, &["checkout", "-q", "-b", "side"]);
        std::fs::write(mine.join("s"), "s").unwrap();
        git(&mine, &["add", "s"]);
        git(&mine, &["commit", "-q", "-m", "side"]);
        git(&mine, &["checkout", "-q", "main"]);
        // Cloned while the upstream was empty, so origin/HEAD was never set.
        git(&mine, &["remote", "set-head", "origin", "main"]);

        let d = deletables(&mine, head).unwrap();
        let labels: Vec<String> = d.entries.iter().map(|(e, _)| format!("{} {}", e.label().0, e.label().1)).collect();
        assert_eq!(
            labels,
            [
                "branch feat",
                "remote branch origin/feat",
                "remote branch origin/main",
                "tag v1",
                "remote tag v1 on origin",
                "tag v2",
            ]
        );
        let warnings: Vec<_> = d.entries.iter().filter_map(|(e, w)| Some((e.label().1, w.clone()?))).collect();
        assert_eq!(warnings, [("origin/main".to_string(), "the default branch on origin".to_string())]);
        assert!(d.notes.iter().any(|n| n.starts_with("main is checked out")), "{:?}", d.notes);

        let side = Repository::open(&mine).unwrap().revparse_single("side").unwrap().id();
        let d = deletables(&mine, side).unwrap();
        assert_eq!(d.entries, [(Deletion::LocalBranch("side".into()), Some("not merged into HEAD".into()))]);

        for what in [
            Deletion::LocalBranch("feat".into()),
            Deletion::RemoteBranch { remote: "origin".into(), branch: "feat".into() },
            Deletion::LocalTag("v1".into()),
            Deletion::RemoteTag { remote: "origin".into(), tag: "v1".into() },
        ] {
            delete(&mine, &what).unwrap();
        }
        let local = git(&mine, &["for-each-ref", "--format=%(refname)"]);
        assert!(!local.contains("refs/heads/feat") && !local.contains("refs/remotes/origin/feat"), "{local}");
        assert!(!local.contains("refs/tags/v1") && local.contains("refs/tags/v2"), "{local}");
        let remote = git(&up, &["for-each-ref", "--format=%(refname)"]);
        assert!(!remote.contains("refs/heads/feat") && !remote.contains("refs/tags/v1"), "{remote}");

        // A remote-tracking ref whose branch is gone on the remote is only removed here.
        git(&mine, &["update-ref", "refs/remotes/origin/gone", "HEAD"]);
        let said = delete(&mine, &Deletion::RemoteBranch { remote: "origin".into(), branch: "gone".into() }).unwrap();
        assert!(said.contains("already gone"), "{said}");
        assert!(!git(&mine, &["for-each-ref"]).contains("origin/gone"));
    }

    #[test]
    fn no_upstream_fails() {
        let (_tmp, mine, _theirs) = setup();
        git(&mine, &["checkout", "-q", "-b", "lonely"]);
        assert!(matches!(pull(&mine).unwrap(), PullOutcome::Failed));
    }
}
