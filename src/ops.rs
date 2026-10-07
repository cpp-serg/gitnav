//! Operations that change the repository. They run the git CLI: libgit2 has no pull, no
//! rebase with autostash, and does not use git's credential helpers or ssh setup.

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
    fn no_upstream_fails() {
        let (_tmp, mine, _theirs) = setup();
        git(&mine, &["checkout", "-q", "-b", "lonely"]);
        assert!(matches!(pull(&mine).unwrap(), PullOutcome::Failed));
    }
}
