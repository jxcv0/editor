# editor

`editor` is an experimental, modal terminal editor written entirely in Rust.
It targets a focused Linux/Rust workflow: fast local editing, built-in project
navigation, a small discoverable leader menu, and a supervised optional
`rust-analyzer` process—without Vim/Neovim runtimes or an embedded scripting
language.

> **Status:** early MVP, not a finished release. The repository has tested text,
> modal, terminal, search, persistence, and process-protocol building blocks,
> but it does not yet satisfy every release criterion in [DESIGN.md](DESIGN.md).
> See [Integration status](#integration-status) before relying on it for
> irreplaceable work.

## Build

The current target is Linux in a UTF-8 terminal. Building the locked dependency
set requires Rust 1.88 or newer.

```sh
cargo test
cargo build --release

# Local budget smoke check (not a substitute for end-to-end startup profiling)
cargo run --release --bin editor-bench
```

The optimized binary is `target/release/editor`. Release builds enable thin
LTO, one codegen unit, and symbol stripping.

## Install

Install the local package's editor binary with Cargo:

```sh
cargo install --path . --locked --bin editor --force
```

Cargo places it at `~/.cargo/bin/editor` by default. Ensure `~/.cargo/bin` is
listed in your shell's `PATH`, then `editor .` works from any directory. The
`--force` flag also makes this command replace an older installed development
copy.

## Run

```sh
# Empty scratch buffer
target/release/editor

# One or more files
target/release/editor src/main.rs Cargo.toml

# Select a project root and open its explorer
target/release/editor .

# Start at a one-based line and column
target/release/editor +42:5 src/main.rs

# Read standard input into a scratch buffer
printf 'temporary text\n' | target/release/editor -

# Do not restore project session metadata
target/release/editor --no-session .
```

Use `editor --help` for the accepted CLI shape and `editor --version` for the
package version. A directory argument establishes the initial project and opens
the explorer. Project-root preference is a containing Cargo workspace, then the
nearest Cargo package, then a Git root, then the supplied directory/current
working directory. Standard-input text has no destination until explicitly
saved with `:w PATH` or `:saveas PATH`. Nonempty stdin content is marked dirty,
so a normal quit protects it even before the first edit. Options may be
intermixed with paths; use `--` before a path beginning with `-` or resembling
`+LINE`. A startup position is currently applied only to the first file target.

## Essential keys

The editor starts in Normal mode. A minimal first session is:

- `i` to insert and `Esc` to return to Normal mode.
- `h j k l`, `w b e`, `0 ^ $`, `gg`, and `G` to move.
- `d`, `c`, or `y` plus a motion; counts compose (`2dw` and `d2w`).
- `u` and `Ctrl-r` to undo and redo; `.` repeats a supported last change.
- `/` or `?` to search; `n`/`N` repeat.
- `v`, `V`, and `Ctrl-v` for character, line, and early block selection.
- `:w` to save; `:q` to close the current pane or quit from the last pane;
  `:wq` to save and close that pane; `:bd` to delete a buffer; `:qa` to quit all.
- `<Space>` for the keyboard-driven command menu; `<Space><Space>` finds files
  and `<Space>e` toggles the explorer.
- `` Ctrl-` `` or `<Space>t` to open or hide the integrated terminal.
- `<Space>cc` to run `cargo check` and `<Space>cw` to watch it in a right-side
  panel that re-runs whenever a Rust file is saved.

The exact supported grammar, leader bindings, Ex commands, picker controls,
and intentional Vim deviations are documented in
[docs/COMMANDS.md](docs/COMMANDS.md). In particular, this is not full Vim
emulation; do not assume an undocumented Vim command is available.

## Saving and text safety

Only valid UTF-8 files are opened. Editing operates on extended grapheme
clusters, and buffer/LSP position conversions distinguish grapheme, byte, and
UTF-16 coordinates.

Normal saves reject a file that changed on disk since it was opened or last
saved. While idle, clean buffers are checked for external changes every two
seconds and reloaded on the foreground thread; modified buffers are left
untouched. Where the platform permits, saving uses a same-directory atomic
replacement while preserving the existing line endings, final-newline state,
Unix permissions, and symlink target. `:w!`, `:e!`, `:bd!`, and `:q!` are the
explicit force/discard paths; review the command reference before using them.
Normal `:q` protects a modified buffer's last visible pane; another pane showing
the same buffer can close safely. `:q!` discards that buffer only when closing
its last view. Other unsaved buffers keep the editor open; `:qa!` explicitly
discards all and exits.

Undo retains changed bytes rather than a complete document per edit. Each
buffer has both an entry limit and a retained-change byte limit (64 MiB by
default). Reaching either limit starts a new history with the latest change;
a single change exceeding the byte limit stays applied but clears history and
cannot be undone. An open transaction retains its rollback data until it ends.

During periodic maintenance, dirty buffers are queued to private, versioned
recovery records; named buffers use a path-derived key and scratch/stdin
buffers use a project-local scratch key. Live revisions are journaled while
Insert mode remains open. Later maintenance removes records for
buffers observed clean, and normal shutdown drains the queued journal work. On launch
the editor reports how many records exist, but it does not yet provide a
recovery-selection or application UI. Clean exit writes content-free session
metadata. A project launch can asynchronously restore named files, the active
named buffer, per-pane cursors/viewports, and explorer open/width/expanded
state. Version 1 does not store exact split topology or orientation, so multiple
saved panes are rebuilt as a deterministic right-deep vertical layout. Restore
is skipped if editing has already changed the initial scratch buffer. The MVP
should not be treated as a substitute for version control or backups.

## Configuration

Configuration is optional TOML with schema version `1`. The library merges:

1. built-in defaults;
2. `$XDG_CONFIG_HOME/editor/config.toml` (normally
   `~/.config/editor/config.toml`); and
3. `<project-root>/.editor.toml`.

Project configuration cannot change external-tool settings unless the caller
has explicitly trusted it. The current CLI does not provide a persistent
project-trust workflow, so project-local `[tools]` overrides are ignored.
Unknown fields, invalid `#rrggbb` colors, unsupported schema versions,
`tab_width` outside 1–16, and `explorer_width` outside 16–120 are errors.

A complete example, showing the built-in values, is:

```toml
schema_version = 1

[editor]
tab_width = 4
insert_spaces = true
format_on_save = false
persistent_undo = true

[ui]
relative_numbers = true
true_color = true
explorer_width = 30
show_hidden = false
show_ignored = false

[ui.theme]
background = "#101619"
foreground = "#dce4e3"
muted = "#77858a"
accent = "#8bd5b6"
status = "#1b252b"
error = "#f7768e"
warning = "#e0af68"
info = "#8bbfd8"
selection = "#2b4548"

[tools.rust_analyzer]
path = "rust-analyzer"
args = []

[tools.cargo]
path = "cargo"
args = []

[limits]
max_file_bytes = 104857600
large_file_bytes = 10485760
message_history = 256
undo_steps = 1000
undo_bytes = 67108864
search_results = 2000
tool_message_bytes = 8388608

[keymap]
```

`:reloadconfig` keeps the previous valid configuration if reloading fails and
replaces the typed configuration when it succeeds. `:config` currently reports
only the schema and loaded source paths. The `keymap`, `format_on_save`,
`persistent_undo`, and `true_color` settings are part of the typed schema, but
custom keymap application, persisted undo,
format-on-save, and a tested 256-color fallback are not complete yet. Reloading
does not rebuild existing undo/explorer state or already-running integration
workers, so some UI/limit settings and tool path, argument, and protocol-limit
changes require restarting the editor.

## Integrated terminal

`` Ctrl-` `` or `<Space>t` toggles a bottom terminal panel and starts `$SHELL` in
the project root the first time it is opened. `:terminal` and `:term` are
aliases. The PTY session keeps running while the panel is hidden. While the
terminal has focus, keys and bracketed paste go directly to the shell, except
`` Ctrl-` ``, which hides the panel; press `Ctrl-\` to return to the editor
without hiding it and `Ctrl-w j` to focus a visible terminal again.
`Shift-PageUp`/`Shift-PageDown` scroll by one page and `Shift-Home`/`Shift-End`
jump to the top/bottom of the bounded scrollback.

Most terminals send `` Ctrl-` `` as the same NUL byte as `Ctrl-Space`, so either
key toggles the panel, and the shell no longer receives `Ctrl-Space`. Insert
mode is the exception: there `Ctrl-Space` still requests completion, so leave
Insert mode before using `` Ctrl-` `` unless your terminal reports it distinctly.

The terminal supports ANSI/256/RGB colors, interactive cursor movement, PTY
resizing, and bounded device/cursor/color capability replies used by modern
shells. It is an explicitly requested interactive shell with the same
permissions and environment as the editor process, not a sandbox or a task
runner. Closing the editor closes the PTY and terminates its shell.

## Optional external tools

No external tool is required for editing, and the editor does not download,
update, or invoke one through a shell.

### `cargo check`

`<Space>cc` (or `:check`) runs `cargo check --message-format=json` in the
project root and shows the result in a panel docked on the right. `<Space>cw`
toggles that panel; while it is visible, saving a `.rs`, `Cargo.toml`, or
`Cargo.lock` file re-runs the check, cancelling any run still in progress.
Hiding the panel stops watching. Errors are listed before warnings, each with
its location, primary label, and rustc notes; the previous results stay on
screen until a new run finishes. `Ctrl-w l` (or `Ctrl-l`) from the rightmost
pane focuses the panel, where `j`/`k` select an entry, `Enter` jumps to its
source location, and `r` re-runs the check.

`tools.cargo.path` selects the cargo executable and `tools.cargo.args` are
appended to the command, for example `["--all-targets"]`; both are read when
each run starts. Like any cargo build, a check runs the project's build scripts
and procedural macros, so it only starts after one of these explicit commands.

### `rust-analyzer`

Install `rust-analyzer` separately and ensure it is on `PATH`, or set
`tools.rust_analyzer.path`. One client starts asynchronously for the selected
project root, speaks bounded JSON-RPC/LSP over stdio, tracks versioned document
snapshots, and exposes lifecycle/failure status. A missing, slow, malformed, or
crashed server leaves local editing available. `<Space>cR`, `:rarestart`, or
`:lsprestart` manually restarts it; the leader action remains available when
the server is failed or not ready.

The transport advertises completion, signature, hover, navigation, references,
rename, actions, formatting, and inlay-hint capabilities. The current runtime
handles diagnostics, including severity-colored gutter markers and Error
Lens-style messages beside affected source lines; manual completion; hover text;
definition/declaration/type/implementation/reference locations, with source-code
previews and exact-column navigation in the `gr` reference picker; active-buffer
formatting; active-buffer rename edits; and basic symbol/action result lists. It
uses live text revisions to send full document changes while an Insert-mode undo
transaction is still open. Disk-backed `rustc`/Clippy results are hidden while
the buffer is dirty, and successful writes notify the server so check-on-save
results refresh. The client does not expand completion snippets and does not
completely apply multi-file workspace edits or code actions.
Request cancellation and several presentation details remain incomplete. See
[docs/COMMANDS.md](docs/COMMANDS.md).

## Integration status

“Implemented” below means code and focused tests exist. It does not imply that
all of `DESIGN.md`'s end-to-end or performance release criteria have passed.

| Area | Current status |
| --- | --- |
| UTF-8 buffer, grapheme edits, transactions, branching undo, atomic/conflict-aware saves | Implemented with regression tests. Undo stores byte-range changes with entry and byte limits; grapheme indexes are cached per changed line. The line-vector text model still needs broader large-file performance validation. |
| Modal grammar, registers/macros, search, Visual modes, panes, buffers, leader registry | Usable MVP subset with known deviations documented in `docs/COMMANDS.md`. |
| Terminal frontend | Raw mode, bracketed paste, resize events, local syntax colors, gutters/status/messages (including modified, large-file, and loaded read-only flags), split/explorer/picker/leader drawing, horizontal cursor tracking, Unicode cell widths, changed-cell output, and a toggled PTY-backed terminal panel are implemented. Mouse forwarding, terminal selection/copy, and suspend/resume remain. |
| Project discovery, finder, and text/regex search | Root selection is a synchronous ancestor walk before the first frame. Full-tree scanning and text search then run in background threads, stream through bounded cancellable queues, and honor hidden/ignored controls. The interactive grep picker uses regex mode and cancels the previous task when its query changes; previews are not yet rendered. Opening the file finder rescans the project in the background (at most every two seconds) without clearing its index, adding new files and dropping removed ones; saved files are added immediately. |
| Explorer | Expandable directory tree with lazy background loading, keyboard navigation, active-file reveal, hidden/ignored filters, and session-persisted expansion. A save re-lists its directory at once, and cached directories whose modification time changes, for example after a file is created in the terminal, are re-listed within about two seconds while idle. Confirmed create/rename/delete operations remain. |
| `rust-analyzer` | Supervised asynchronous transport, initialization, manual restart, versioned full-text sync, typed requests, diagnostics, basic response UI/edit application, size bounds, and failure status are wired. Snippets, code-action execution, multi-file rename, inlay rendering, request cancellation, and capability-specific disabled states remain. |
| `cargo check` panel | Explicit and save-triggered runs, JSON diagnostic parsing, a right-side panel with errors first, and jumps to rustc locations are implemented with process-level tests. Runs share cargo's build-directory lock with rust-analyzer's own check-on-save, so one can briefly wait for the other. |
| Recovery and sessions | Dirty named and scratch/stdin buffers are journaled asynchronously, queued journal work is joined at shutdown, and content-free named-file sessions restore active selection, pane cursor/viewports, and explorer metadata. Recovery choice/application, unnamed session buffers, exact split topology/orientation, explicit-discard cleanup, and persistent undo are not release-complete. |
| Performance/release evidence | `editor-bench` reports warm 1 MiB buffer-open and component p95 checks for insertion, long-word movement, whole-file indentation, long-line rendering, and finder input. It does not measure process launch through terminal flush or runtime maintenance, has no target-laptop/CI baseline yet, and therefore does not prove the complete 50 ms/8 ms release budgets. Broader fuzz/property coverage remains release work. |

For module ownership and boundaries, see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The dependency/native-boundary
review required by Design Requirement 1 is in
[docs/DEPENDENCIES.md](docs/DEPENDENCIES.md).
