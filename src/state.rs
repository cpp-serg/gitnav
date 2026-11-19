//! Choices made in the TUI that last from one run to the next, kept in
//! `$XDG_STATE_HOME/gitnav/state.toml` (`~/.local/state/gitnav/state.toml` by default).
//! Unlike the config file, gitnav writes this one itself.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::ops::DiffTool;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct State {
    /// What `d` shows a diff with (`D` switches it).
    pub diff_tool: DiffTool,
}

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("gitnav").join("state.toml"))
}

/// The saved state; the defaults if there is none or it cannot be read.
pub fn load() -> State {
    default_path().map(|p| load_from(&p)).unwrap_or_default()
}

fn load_from(path: &Path) -> State {
    std::fs::read_to_string(path).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default()
}

pub fn save(state: &State) -> Result<()> {
    let path = default_path().context("no state directory: neither XDG_STATE_HOME nor HOME is set")?;
    save_to(&path, state)
}

fn save_to(path: &Path, state: &State) -> Result<()> {
    let write = || -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write a new file and rename it over the old one, so a reader never sees half of it.
        let new = path.with_extension("toml.new");
        std::fs::write(&new, toml::to_string(state)?)?;
        std::fs::rename(&new, path)?;
        Ok(())
    };
    write().with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gitnav").join("state.toml");
        assert_eq!(load_from(&path), State::default(), "no file yet");

        let state = State { diff_tool: DiffTool::Difft };
        save_to(&path, &state).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "diff-tool = \"difft\"\n");
        assert_eq!(load_from(&path), state);

        std::fs::write(&path, "diff-tool = \"nope\"\n").unwrap();
        assert_eq!(load_from(&path), State::default(), "a bad file gives the defaults");
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_from(&path), State::default());
    }
}
