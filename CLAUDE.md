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
  as row indices, git-`%D`-style decorations, a ref fingerprint, the HEAD row). While tracked
  files differ from HEAD (`git diff HEAD`, submodules only by their commit), a `<WTREE>` row with
  the all-zero id (`worktree_id`) sits on top of HEAD as its child; the fingerprint counts those
  files, so watching adds and drops it. `ops::Change` turns it into `git diff` against HEAD or
  another commit. The repository's worktrees (`worktrees`, the main one and the one shown
  included) are starting points of the walk, as with `git log --all`, and decorate their HEAD
  commits as `WT: <path>` (`RefKind::Worktree`, drawn in another colour; `full` is none, so `x`
  leaves them). `merge_kind` works out what a merge did (`MergeKind`: forced where a fast-forward
  was possible, clean, or resolved by hand) by merging its parents again in memory; nothing is
  written. libgit2's answers match `git show --remerge-diff` on ggsn. `simplify(history, keep)`
  keeps a subset of rows and rejoins each kept commit to its nearest kept ancestors; real parent
  edges are never pruned, only joins made across hidden commits. Refs-only, the text filter and
  head-only all use it.
- **`order.rs`**: `GitTopo` and `Date` reproduce git's `revision.c` exactly (tests compare
  against `git log`). `Topo` is git's stack walk plus a search for the branch blocking the current
  line, so a blocked parent comes as soon as possible. Topo and Date begin with the layout head
  and what leads to it. In first-parent mode only first parents order and draw anything, which
  matches git when it uses a commit-graph file.
- **`graph.rs`** lays out lanes, one `Row` per commit: the commit's line, and under it the
  connector lines where lines change column, as in `git log --graph` (`╲`/`╱` for a step to the
  next column; a longer reach is a staircase, one character sideways per line, which a line
  passing by may begin on the commit's line beside its column; git's `_` run only where a
  staircase would cut through another line). `connector` repeats until its height settles, since a
  reach falling back to a run needs fewer lines. Lanes keep their column; the only sideways move
  is the step a line takes to make room for a merge's source next to its target. Inputs besides
  the parents: the *spine* (the head's line, always lane 0, which nothing else may use) and
  *heavy* rows (the head and everything it reaches, drawn with `┃` via `Cell::shown`, and bold
  where no heavy glyph exists). Lines heading for the same commit join early: a head-history line
  never gives way to an outside one, otherwise the younger joins the older. `Reserved` counts the
  head-history lines open per row so outside lines are placed to the right of them; a line that
  joins the spine under its own row counts on that row only, and an outside line's last row (where
  it comes in from above) does not count. A column whose line ended in a commit is free again for
  a line leaving that commit.
- **`app.rs`** holds UI state and key handling and does no I/O: keys set `reload` or `command`
  and `main.rs` acts on them. Two layers of state:
  - `View` (order, deleted, hidden refs, refs-only, chosen head, first-parent) changes what
    `repo::load` produces, so changing it reloads.
  - The search, text filter (`filter`) and head-only are applied in `App` from `base` (the
    loaded history) without reloading.

  `swap_rows` keeps the cursor, marks and visual anchor on the same commits across any change of
  rows. `offset` is a row but `height` is screen lines, and a row can take several: scrolling
  goes through `shown_from`, `top_for` and `bottom`. `relayout` computes the spine and heavy set
  and calls `graph::layout`. `HELP` is the one key table: add a key's line there along with its
  handler.
- **`ui.rs`**: `History::line(row)` is the text that search and filter match. The text spans in
  `commit_line` must concatenate to exactly that string (a test checks it), because highlight
  positions are character offsets into it. Anything drawn in addition (the merge count after
  the hash) is inserted after highlighting with `insert_at`.
- **`ops.rs`** is the only place that runs the git CLI at runtime: pull with autostash and rebase,
  and rebase abort (libgit2 has no equivalent with git's credential handling; `main.rs` leaves the
  alternate screen while they run so git can prompt), a `Change` (one commit as `git show`, or two
  as `git diff`) for Enter and `d`, and deleting branches and tags locally and on remotes
  (Delete). A change goes to the `DiffTool` chosen with `D`: diffnav (run outside the TUI),
  difftastic as git's external diff, or git through its configured pager; the last two land in
  `app::Pager`, with colours that `ansi.rs` turns into styled lines. A tool not on `PATH` falls
  back to git. `d` diffs from an ancestor to its descendant, else from the second selected commit
  to the first (`diff_order`). Linked worktrees at the commit are entries of their own
  (`Deletion::Worktree`, `git worktree remove --force`); a branch checked out in one goes only
  with it, so ticking the branch ticks the worktree and unticking the worktree unticks the branch,
  and worktrees are removed first. Ticked worktrees bring a second question; uncommitted tracked
  changes there show in red and take `Y` (shift). Anything that talks to a remote while the TUI
  owns the terminal goes through `ops::batch`, which stops git and ssh from prompting; pushes run
  outside the TUI instead.
- **`config.rs`**: settings come from the config file
  (`$XDG_CONFIG_HOME/gitnav/config.toml`), then `$GITNAV_OPTS`, then the command line, later
  winning (`--hide` patterns add up). A new option needs a `--x`/`--no-x` pair in `Cli`, a
  kebab-case key in `FileConfig` (unknown keys are rejected), the `View` field, and a line in
  `after_help`.
- **`state.rs`**: what the TUI keeps from one run to the next (the diff tool), in
  `$XDG_STATE_HOME/gitnav/state.toml`, which gitnav writes itself; the config file stays the
  user's.
- **Merge kinds**: worked out only for the merges on screen, in a worker thread in `main.rs`
  (`merge_worker`), never in the key path: after each draw `App::merges_to_classify` names
  the new ones, and answers land in `App::merge_kinds`, kept by commit id across reloads.
  `ui::merge_glyph` draws ⊙ ⊕ ⊗. Enter on a merge (git or difft) shows `ops::merge_text`: the
  message, the commits brought in, what was done by hand (only if anything), and the diff
  against the first parent; `m` shows only the hand-made part.
- **Watching**: `main.rs` polls `repo::current_fingerprint` (all refs and HEAD) every interval
  and reloads when it differs from the one recorded at load.
