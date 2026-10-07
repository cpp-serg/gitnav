//! Operations that run the git CLI: pull and rebase (libgit2 has no pull, no rebase with
//! autostash, and does not use git's credential helpers or ssh setup), and `git show`.

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
    let dir = workdir_or_gitdir(path)?;
    let pager = configured_pager(&dir).and_then(|p| filter_pager(&p, width));
    show_through(&dir, oid, width, pager.as_deref())
}

/// `git show` in `dir`, piped through `pager` if there is one.
fn show_through(dir: &Path, oid: git2::Oid, width: usize, pager: Option<&str>) -> Result<String> {
    let mut git = Command::new("git");
    git.args(["--no-pager", "show", "--color=always"]).arg(oid.to_string()).current_dir(dir);
    let Some(pager) = pager else {
        let out = git.stdin(Stdio::null()).output().context("running git show")?;
        if !out.status.success() {
            bail!("git show failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    };

    let mut show = git
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git show")?;
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
    let shown = show.wait_with_output().context("running git show")?;
    if !shown.status.success() {
        bail!("git show failed: {}", String::from_utf8_lossy(&shown.stderr).trim());
    }
    if !out.status.success() {
        bail!("the pager ({pager}) failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The pager git would use for `git show` in `dir`: `pager.show` if it names one (none if it
/// is false), else `git var GIT_PAGER` ($GIT_PAGER, core.pager, $PAGER, git's default).
fn configured_pager(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    match git(&["config", "--type=bool-or-str", "--get", "pager.show"]).as_deref() {
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
        let text: String = crate::ansi::to_lines(&show_through(&mine, head, 100, None).unwrap())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n")
            .collect();
        assert!(text.contains(&format!("commit {head}")), "{text}");
        assert!(text.contains("\n+base\n"), "{text}");
    }

    #[test]
    fn show_through_a_pager() {
        let (_tmp, mine, _theirs) = setup();
        let head = Repository::open(&mine).unwrap().head().unwrap().target().unwrap();
        let text = show_through(&mine, head, 100, Some("sed s/^/X/")).unwrap();
        assert!(!text.is_empty() && text.lines().all(|l| l.starts_with('X')), "{text}");
        assert!(show_through(&mine, head, 100, Some("false")).is_err(), "a failing pager is reported");
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
        assert_eq!(configured_pager(&mine), None);
        git(&mine, &["config", "pager.show", "tac"]);
        assert_eq!(configured_pager(&mine).as_deref(), Some("tac"));
    }

    #[test]
    fn no_upstream_fails() {
        let (_tmp, mine, _theirs) = setup();
        git(&mine, &["checkout", "-q", "-b", "lonely"]);
        assert!(matches!(pull(&mine).unwrap(), PullOutcome::Failed));
    }
}
