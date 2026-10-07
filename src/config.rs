//! Settings: from the config file, then `GITNAV_OPTS`, then the command line; later wins.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;

use crate::order::Order;
use crate::repo::{Deleted, View};

/// The environment variable holding default command-line options.
pub const OPTS_VAR: &str = "GITNAV_OPTS";

const DEFAULT_INTERVAL: u64 = 5;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Navigate a git repository's revision graph",
    args_override_self = true,
    after_help = "Options are read from the config file first, then from $GITNAV_OPTS \
                  (command-line options separated by spaces), then from the command line; \
                  later ones win.\n\nConfig file: $XDG_CONFIG_HOME/gitnav/config.toml \
                  (~/.config/gitnav/config.toml), with the keys order = \"topo\"|\"date\", \
                  deleted = \"hide\"|\"reflog\"|\"all\", watch = true|false, watch-interval = SECS."
)]
struct Cli {
    /// Repository to show [default: the current directory]
    path: Option<PathBuf>,

    /// Keep each line of history together, and show a blocked line's parent as soon as
    /// possible [default]
    #[arg(long, overrides_with_all = ["date_order", "git_topo_order"])]
    topo_order: bool,

    /// Order like `git log --date-order`: newest commit first, parents after children
    #[arg(long, overrides_with_all = ["topo_order", "git_topo_order"])]
    date_order: bool,

    /// Order exactly like `git log --topo-order` (and `git log --graph`)
    #[arg(long, overrides_with_all = ["topo_order", "date_order"])]
    git_topo_order: bool,

    /// Leave out the refs matching PATTERN, and the commits only they reach; repeatable.
    /// `*` and `?` glob; a pattern starting with refs/ (or HEAD) matches full ref names,
    /// others match short names such as main, origin/x or v1
    #[arg(long, value_name = "PATTERN")]
    hide: Vec<String>,

    /// Show only commits with a ref (branch, tag, HEAD, stash), joined to their nearest such
    /// ancestors
    #[arg(long, overrides_with = "no_refs_only")]
    refs_only: bool,

    /// Show every commit [default]
    #[arg(long, overrides_with = "refs_only")]
    no_refs_only: bool,

    /// Follow only first parents, as `git log --first-parent`; merges are marked
    #[arg(long, overrides_with = "no_first_parent")]
    first_parent: bool,

    /// Follow all parents [default]
    #[arg(long, overrides_with = "first_parent")]
    no_first_parent: bool,

    /// Also show commits no ref reaches: those in reflogs, or every commit object
    #[arg(long, value_enum, value_name = "WHICH")]
    deleted: Option<Deleted>,

    /// Reload when the repository's refs change [default]
    #[arg(long, overrides_with = "no_watch")]
    watch: bool,

    /// Do not watch the repository for changes
    #[arg(long, overrides_with = "watch")]
    no_watch: bool,

    /// Seconds between checks for changes [default: 5]
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    watch_interval: Option<u64>,

    /// Config file to read instead of the default one
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct FileConfig {
    order: Option<Order>,
    deleted: Option<Deleted>,
    hide: Vec<String>,
    refs_only: Option<bool>,
    first_parent: Option<bool>,
    watch: Option<bool>,
    watch_interval: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub struct Settings {
    pub path: PathBuf,
    pub view: View,
    pub watch: bool,
    pub interval: Duration,
}

impl Settings {
    /// Reads the settings from the real environment, command line and config file.
    /// Exits the process for `--help`, `--version` and command-line errors, as clap does.
    pub fn load() -> Result<Settings> {
        let cli = Cli::parse_from(args_with_env(
            std::env::args_os().collect(),
            std::env::var_os(OPTS_VAR),
        ));
        let file = match &cli.config {
            Some(path) => Some(read_config(path)?),
            None => match default_config_path() {
                Some(path) if path.exists() => Some(read_config(&path)?),
                _ => None,
            },
        };
        resolve(cli, file.unwrap_or_default())
    }
}

/// Puts the options from the environment variable right after the program name, so that the
/// real command line comes later and overrides them.
fn args_with_env(mut args: Vec<OsString>, env: Option<OsString>) -> Vec<OsString> {
    if let Some(env) = env {
        let words = env.to_string_lossy().split_whitespace().map(OsString::from).collect::<Vec<_>>();
        let at = args.len().min(1);
        args.splice(at..at, words);
    }
    args
}

fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("gitnav").join("config.toml"))
}

fn read_config(path: &Path) -> Result<FileConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    parse_config(&text).with_context(|| format!("in config file {}", path.display()))
}

fn parse_config(text: &str) -> Result<FileConfig> {
    let config: FileConfig = toml::from_str(text)?;
    if config.watch_interval == Some(0) {
        bail!("watch-interval must be at least 1 second");
    }
    Ok(config)
}

/// A `--x` / `--no-x` pair: whichever was given (clap keeps only the last), or neither.
fn flag(yes: bool, no: bool) -> Option<bool> {
    match (yes, no) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

fn resolve(cli: Cli, file: FileConfig) -> Result<Settings> {
    let order = match (cli.topo_order, cli.date_order, cli.git_topo_order) {
        (true, _, _) => Some(Order::Topo),
        (_, true, _) => Some(Order::Date),
        (_, _, true) => Some(Order::GitTopo),
        _ => None,
    };
    let watch = flag(cli.watch, cli.no_watch);
    Ok(Settings {
        path: cli.path.unwrap_or_else(|| PathBuf::from(".")),
        view: View {
            order: order.or(file.order).unwrap_or_default(),
            deleted: cli.deleted.or(file.deleted).unwrap_or_default(),
            hide: file.hide.into_iter().chain(cli.hide).collect(),
            hiding: true,
            refs_only: flag(cli.refs_only, cli.no_refs_only).or(file.refs_only).unwrap_or(false),
            head: None,
            first_parent: flag(cli.first_parent, cli.no_first_parent).or(file.first_parent).unwrap_or(false),
        },
        watch: watch.or(file.watch).unwrap_or(true),
        interval: Duration::from_secs(
            cli.watch_interval.or(file.watch_interval).unwrap_or(DEFAULT_INTERVAL),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(env: Option<&str>, args: &[&str], file: &str) -> Settings {
        let mut argv = vec![OsString::from("gitnav")];
        argv.extend(args.iter().map(OsString::from));
        let cli = Cli::try_parse_from(args_with_env(argv, env.map(OsString::from))).unwrap();
        resolve(cli, parse_config(file).unwrap()).unwrap()
    }

    #[test]
    fn defaults() {
        let s = settings(None, &[], "");
        assert_eq!(s.path, PathBuf::from("."));
        assert_eq!(s.view, View::default());
        assert!(s.watch);
        assert_eq!(s.interval, Duration::from_secs(5));
    }

    #[test]
    fn file_then_env_then_command_line() {
        let file = "order = \"date\"\ndeleted = \"reflog\"\nwatch = false\nwatch-interval = 30\n";
        let s = settings(None, &[], file);
        assert_eq!((s.view.order, s.view.deleted, s.watch), (Order::Date, Deleted::Reflog, false));
        assert_eq!(s.interval, Duration::from_secs(30));

        let s = settings(Some("--topo-order --watch --watch-interval 2"), &[], file);
        assert_eq!((s.view.order, s.watch, s.interval), (Order::Topo, true, Duration::from_secs(2)));

        let s = settings(Some("--topo-order --watch-interval 2"), &["--date-order", "--no-watch", "--watch-interval", "9", "repo"], file);
        assert_eq!((s.view.order, s.watch, s.interval), (Order::Date, false, Duration::from_secs(9)));
        assert_eq!(s.path, PathBuf::from("repo"));
    }

    #[test]
    fn hide_patterns_add_up() {
        let s = settings(Some("--hide a"), &["--hide", "b", "--git-topo-order"], "hide = [\"f\"]");
        assert_eq!(s.view.hide, ["f", "a", "b"]);
        assert_eq!(s.view.order, Order::GitTopo);
        let s = settings(None, &[], "order = \"git-topo\"");
        assert_eq!(s.view.order, Order::GitTopo);
    }

    #[test]
    fn refs_only() {
        assert!(!settings(None, &[], "").view.refs_only);
        assert!(settings(None, &[], "refs-only = true").view.refs_only);
        assert!(!settings(None, &["--no-refs-only"], "refs-only = true").view.refs_only);
        assert!(settings(Some("--refs-only"), &[], "").view.refs_only);
    }

    #[test]
    fn first_parent() {
        assert!(!settings(None, &[], "").view.first_parent);
        assert!(settings(None, &[], "first-parent = true").view.first_parent);
        assert!(!settings(None, &["--no-first-parent"], "first-parent = true").view.first_parent);
    }

    #[test]
    fn last_flag_wins() {
        let s = settings(None, &["--no-watch", "--watch", "--date-order", "--topo-order"], "");
        assert_eq!((s.watch, s.view.order), (true, Order::Topo));
    }

    #[test]
    fn bad_config() {
        assert!(parse_config("colour = 1").is_err());
        assert!(parse_config("watch-interval = 0").is_err());
        assert!(parse_config("order = \"sideways\"").is_err());
    }
}
