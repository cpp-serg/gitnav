//! Operations that run the git CLI: pull and rebase (libgit2 has no pull, no rebase with
//! autostash, and does not use git's credential helpers or ssh setup), `git show`, and
//! deleting branches and tags, locally and on remotes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use git2::{Repository, RepositoryState};

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

/// `git show` of one commit as text, passed through the pager git would use for it (delta,
/// say), with its colours. `width` is the room it gets, for pagers that lay out by width.
pub fn show(path: &Path, oid: git2::Oid, width: usize) -> Result<String> {
    colored(path, &["show".into(), oid.to_string()], width)
}

/// `git diff from to` as text, passed through the pager as `show` does.
pub fn diff(path: &Path, from: git2::Oid, to: git2::Oid, width: usize) -> Result<String> {
    colored(path, &["diff".into(), from.to_string(), to.to_string()], width)
}

/// The output of the git command `args` (`args[0]` names it), coloured and passed through the
/// pager git would use for that command.
fn colored(path: &Path, args: &[String], width: usize) -> Result<String> {
    let dir = workdir_or_gitdir(path)?;
    let pager = configured_pager(&dir, &args[0]).and_then(|p| filter_pager(&p, width));
    run_through(&dir, args, width, pager.as_deref())
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
/// job the commit view does itself. Delta is told the width and not to page.
fn filter_pager(pager: &str, width: usize) -> Option<String> {
    let program = pager.split_whitespace().next()?;
    match Path::new(program).file_name()?.to_str()? {
        "less" | "more" | "most" | "lv" | "cat" => None,
        "delta" => Some(format!("{pager} --paging=never --width={width}")),
        _ => Some(pager.to_string()),
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
            Deletion::RemoteBranch { remote, branch } => ("remote branch", format!("{remote}/{branch}")),
            Deletion::LocalTag(t) => ("tag", t.clone()),
            Deletion::RemoteTag { remote, tag } => ("remote tag", format!("{tag} on {remote}")),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Deletion::RemoteBranch { .. } | Deletion::RemoteTag { .. })
    }

    /// The name it sorts by, so that a branch or tag and its remote copies stay together.
    fn name(&self) -> &str {
        match self {
            Deletion::LocalBranch(n) | Deletion::LocalTag(n) => n,
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
            let warning = (!merged).then(|| "not merged into HEAD".to_string());
            entries.push((Deletion::LocalBranch(branch.to_string()), warning));
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
    entries.sort_by(|(a, _), (b, _)| (a.name(), a.is_remote(), a.label()).cmp(&(b.name(), b.is_remote(), b.label())));
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
    fn which_pagers_filter() {
        assert_eq!(filter_pager("delta", 120).as_deref(), Some("delta --paging=never --width=120"));
        assert_eq!(filter_pager("/usr/local/bin/delta --dark", 80).as_deref(), Some("/usr/local/bin/delta --dark --paging=never --width=80"));
        assert_eq!(filter_pager("diff-so-fancy | less", 80).as_deref(), Some("diff-so-fancy | less"));
        assert_eq!(filter_pager("less -RS", 80), None);
        assert_eq!(filter_pager("/usr/bin/more", 80), None);
        assert_eq!(filter_pager("cat", 80), None);
        assert_eq!(filter_pager("", 80), None);
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
