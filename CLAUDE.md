# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

gitnav is a Rust TUI (ratatui + crossterm) for browsing a git repository's revision graph with
vi-style keys. Commit data comes from libgit2 (`git2`); the graph layout and drawing are its own.

## Commands

`.cargo/config.toml` makes `x86_64-unknown-linux-musl` the default target, so every build is a
fully static binary (libgit2 built from its bundled source, without git2's https/ssh features)
that runs on any x86_64 Linux. It needs `rustup target add x86_64-unknown-linux-musl` and
`musl-gcc` (Arch: `pacman -S musl`).

```sh
cargo build                          # debug builds only for now; binary: target/x86_64-unknown-linux-musl/debug/gitnav
cargo test                           # all tests
cargo test graph::tests::octopus     # one test (any substring of the path works)
cargo clippy --all-targets           # expected to be warning-free
cargo run -- [PATH] [OPTIONS]        # `--help` lists the options
```

Opt-in checks against a real repository (ignored by default):

```sh
GITNAV_COMPARE_REPO=<path> cargo test -- --ignored order_matches              # git-topo == git log, also --first-parent
GITNAV_COMPARE_REPO=<path> cargo test -- --ignored --nocapture order_metrics  # lane width per order
```

The `repo` and `ops` tests build throwaway repositories with the `git` CLI. The `ops` tests run
real `git pull --rebase` with the user's git config; the conflict test prints git's hints, which
is expected noise.

## Architecture

Data flows one way: `config::Settings` → `repo::load(path, &View)` → `App` → `ui::draw`.
`main.rs` owns the event loop and all I/O.

- **Rows are always in display order with children before parents.** Every stage (`order`,
  `repo::simplify`, `graph::layout`) relies on it; a parent always has a higher row index.
- **`repo.rs`** walks all refs (except `refs/prefetch/*`) plus HEAD, optionally reflogs or every
  commit object (`Deleted`), sorts with `order::sort`, and builds `History` (commits with parents
  as row indices, git-`%D`-style decorations, a ref fingerprint, the HEAD row). `simplify(history,
  keep)` keeps a subset of rows and rejoins each kept commit to its nearest kept ancestors; real
  parent edges are never pruned, only joins made across hidden commits. Refs-only, the text
  filter and head-only all use it.
- **`order.rs`**: `GitTopo` and `Date` reproduce git's `revision.c` exactly (tests compare
  against `git log`). `Topo` is git's stack walk plus a search for the branch blocking the current
  line, so a blocked parent comes as soon as possible. Topo and Date begin with the layout head
  and what leads to it. In first-parent mode only first parents order and draw anything, which
  matches git when it uses a commit-graph file.
- **`graph.rs`** lays out lanes, one row per commit. Lanes keep their column; the only sideways
  move is the one-row step a line takes to make room for a merge's source next to its target.
  Inputs besides the parents: the *spine* (the head's line, always lane 0, which nothing else
  may use) and *heavy* rows (the head and everything it reaches, drawn with heavy box glyphs via
  `Cell::shown`). Lines heading for the same commit join early: a head-history line never gives
  way to an outside one, otherwise the younger joins the older. `Reserved` counts the
  head-history lines open per row so outside lines are placed to the right of them.
- **`app.rs`** holds UI state and key handling and does no I/O: keys set `reload` or `command`
  and `main.rs` acts on them. Two layers of state:
  - `View` (order, deleted, hidden refs, refs-only, chosen head, first-parent) changes what
    `repo::load` produces, so changing it reloads.
  - The search, text filter (`filter`) and head-only are applied in `App` from `base` (the
    loaded history) without reloading.

  `swap_rows` keeps the cursor, marks and visual anchor on the same commits across any change of
  rows. `relayout` computes the spine and heavy set and calls `graph::layout`. `HELP` is the one
  key table: add a key's line there along with its handler.
- **`ui.rs`**: `History::line(row)` is the text that search and filter match. The text spans in
  `commit_line` must concatenate to exactly that string (a test checks it), because highlight
  positions are character offsets into it. Anything drawn in addition (the merge count after
  the hash) is inserted after highlighting with `insert_at`.
- **`ops.rs`** is the only place that runs the git CLI at runtime: pull with autostash and
  rebase, and rebase abort (libgit2 has no equivalent with git's credential handling; `main.rs`
  leaves the alternate screen while they run so git can prompt), and `git show` for the commit
  view (Enter), whose colours `ansi.rs` turns into styled lines for `app::Pager`.
- **`config.rs`**: settings come from the config file
  (`$XDG_CONFIG_HOME/gitnav/config.toml`), then `$GITNAV_OPTS`, then the command line, later
  winning (`--hide` patterns add up). A new option needs a `--x`/`--no-x` pair in `Cli`, a
  kebab-case key in `FileConfig` (unknown keys are rejected), the `View` field, and a line in
  `after_help`.
- **Watching**: `main.rs` polls `repo::current_fingerprint` (all refs and HEAD) every interval
  and reloads when it differs from the one recorded at load.
