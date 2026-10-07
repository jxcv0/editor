# editor

`editor` is an experimental, modal editor written entirely in Rust, with a
terminal frontend and an optional egui/wgpu desktop frontend.
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
The test suite also uses `git` and Linux PTYs with private temporary repositories.

```sh
cargo test
cargo build --release

# Local budget smoke check (not a substitute for end-to-end startup profiling)
cargo run --release --bin editor-bench
```

The optimized binary is `target/release/editor`. Release builds enable thin
LTO, one codegen unit, and symbol stripping.

### GPU desktop frontend

```sh
cargo build --release --locked --features gui
target/release/editor --gui .
target/release/editor --gui +42:5 src/main.rs
```

The `gui` feature adds a native, DPI-aware egui window rendered by wgpu's Vulkan
backend. It needs a Linux Wayland or X11 desktop and a Vulkan driver. The default
build stays terminal-only; a GUI-enabled binary still uses the terminal unless
passed `--gui`. To install both frontends, add `--features gui` to the install
command below.

The desktop uses a dark charcoal and mint interface with a resizable project
tree, clickable buffer tabs, a command rail, Cargo menu, save/find/terminal
buttons, split panes, and the same syntax, diagnostics, inlays, pickers, Git
views, and PTY as the terminal. Click code to move the cursor, drag to select,
and click a pane or panel to focus it. Wheel input navigates the focused view;
with terminal focus it scrolls history. The editor remains modal: `i` inserts,
Esc returns to Normal, and Space opens commands. Ctrl-P finds files, Ctrl-S
saves, Ctrl-Shift-C copies a Visual selection or last yank, and Ctrl-Shift-V
pastes. Ctrl-V retains block selection. Ctrl-plus/minus/0 adjusts/resets the
editor font size. Dropped files open in buffers. Closing the window protects
all unsaved buffers with a keep-editing/discard dialog.

The GUI uses embedded FiraCode Nerd Font Mono by default, including Nerd Font
symbols and programming ligatures such as `!=`, `=>`, and `->`. No system font
installation is needed. Ligatures preserve individual character positions for
editing and mouse input; they stop at color, selection, and non-ASCII grapheme
boundaries. The font and its SIL Open Font License are in
[assets/fonts](assets/fonts/README.md). The titlebar's **editor** menu also opens
the bundled font license. Terminal-only builds do not embed it;
the terminal emulator controls its own font.

The window shares the editing and integration runtime, including conflict-aware
saves, undo, sessions, and recovery. The canvas is drawn as GPU glyphs and
geometry; source layout and motion still use cell coordinates. Native file
dialogs, terminal mouse selection, and drag selection across panes are not
implemented. Font zoom is session-local. The existing configuration theme
controls document colors; desktop chrome has its own dark palette.

For an explicit headless Vulkan rendering check:

```sh
cargo test --features gui offscreen_desktop_and_diff_render -- --ignored --nocapture
```

This writes desktop, diff, ligature/zoom, and unsaved-close PPM screenshots under
`target/editor-validation/`. It works with Mesa's software Vulkan driver and
does not require a window server. Offscreen rendering and input/lifecycle unit
tests do not replace testing window-system input and IME behavior on a desktop.

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
- `D` (Shift-D) to change from the cursor through end of line and enter Insert
  mode, like `c$`.
- `u` and `Ctrl-r` to undo and redo; `.` repeats a supported last change.
- `/` or `?` to search; `n`/`N` repeat.
- `v`, `V`, and `Ctrl-v` for character, line, and early block selection.
- `:w` to save; `:q` to close the current pane or quit from the last pane;
  `:wq` to save and close that pane; `:bd` to delete a buffer; `:qa` to quit all.
- `<Space>` for the keyboard-driven command menu; `<Space><Space>` finds files
  and `<Space>e` toggles the explorer.
- `` Ctrl-` `` or `:terminal` to open or hide the integrated terminal.
- `<Space>C` for Cargo run, test, build, check, update, Clippy, fmt, doc, clean,
  and cancellation, with output in the right-side panel.
- `<Space>cc` to run `cargo check` and `<Space>cw` to watch it in a right-side
  panel that re-runs whenever a Rust file is saved.
- `<Space>gs` for Git status, `<Space>gd`/`<Space>gD` for unstaged/staged
  diffs, and `<Space>gb` for the last commit affecting the current line.
- `<Space>ua` to toggle smooth scrolling and cursor animation.

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
smooth_scroll = true
cursor_animation = true
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

With `editor.format_on_save = true`, Rust writes request formatting from
`rust-analyzer`, apply the edits to the live buffer as one undo step, and then
save through the usual conflict checks. `:wq` waits for this operation before
closing its pane. A missing server, rejected request, or three-second timeout
falls back to an unformatted save with a message. Editing or switching away
while formatting is pending cancels that save; save again when ready. Newly
named scratch buffers are first saved to establish their LSP document URI.

`:reloadconfig` keeps the previous valid configuration if reloading fails and
replaces the typed configuration when it succeeds. `:config` currently reports
only the schema and loaded source paths. The `keymap`, `persistent_undo`, and
`true_color` settings are part of the typed schema, but custom keymap
application, persisted undo, and a tested 256-color fallback are not complete
yet. Reloading
does not rebuild existing undo/explorer state or already-running integration
workers, so some UI/limit settings and tool path, argument, and protocol-limit
changes require restarting the editor.

Normal-mode navigation uses eased scrolling (100 ms) and cursor movement
(70 ms), with frames scheduled while motion is active. Editing positions
update immediately; entering Insert mode, changing text, and resizing snap
the presentation to the current position. Large jumps animate at most half a
pane of scroll distance. Set `ui.smooth_scroll` and `ui.cursor_animation`
independently to `false` for reduced motion, or toggle both with `<Space>ua`.
The custom terminal renderer works in whole cells, so these are short cell
transitions rather than pixel scrolling. These settings also take effect on
`:reloadconfig`.

## Integrated terminal

`` Ctrl-` `` toggles a bottom terminal panel and starts `$SHELL` in
the project root the first time it is opened. `:terminal` and `:term` are
aliases. The PTY session keeps running while the panel is hidden. While the
terminal has focus, keys and bracketed paste go directly to the shell, except
`` Ctrl-` ``, which hides the panel; press `Ctrl-\` to return to the editor
without hiding it and `Ctrl-w j` to focus a visible terminal again.
`Ctrl-D` at an empty shell prompt exits the shell and closes the panel, returning
focus to the editor. It retains normal EOF behavior inside running programs.
Opening the terminal after shell exit starts a fresh session.
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

### Git

With `git` on `PATH`, the editor reads repository status asynchronously after
the first frame, when the active file changes, and about every two seconds
while idle. The status bar shows the branch and staged/unstaged file counts.
The active saved file gets two gutter columns: staged first, unstaged second;
`+`, `~`, and `-` mean added, changed, and deleted text. Modified buffers hide
these disk-based markers until saved and refreshed. `<Space>gn`/`<Space>gp`
move between changed hunks.

`<Space>gs` or `:git` opens status. Select a file with `j`/`k`, then use `d`
for its unstaged diff, `D` for its staged diff, or `H` for its diff against
HEAD. `s` stages the selected saved file and `u` unstages it; neither writes
buffer text. `o` opens the file, Backspace returns to status, `r` refreshes
status, and `q`/Esc closes the view. Diffs scroll with `j`/`k`, Page Up/Down,
or Ctrl-b/Ctrl-f; `h`/`l` scroll horizontally.
At widths of at least 64 cells, patches show aligned BEFORE/AFTER columns with
independent line numbers, colored changes, and blanks for unmatched lines.
Context stays on the same row on both sides. Smaller views show unified diffs.

From a source buffer, `<Space>gb` shows the last commit for the current saved
line and `<Space>gc` shows that commit's patch. `<Space>ga`/`<Space>gu`
stage/unstage the active file. Stage, unstage, and line-history commands
reject unsaved buffer edits. Diff views inspect saved files and remain
read-only snapshots. Git is optional; missing tools and non-repositories
leave editing available. See [COMMANDS.md](docs/COMMANDS.md#git) for exact
bindings, bounds, and edge cases.

### `cargo check`

The `<Space>C` Cargo menu runs `run`, `check`, `test`, `build`, `update`,
`clippy`, `fmt`, `doc`, and `clean` directly in the project root, without
opening a terminal. `:cargo COMMAND [args]` accepts the same commands and
whitespace-separated literal arguments. Program/test output streams into the
panel; compiler diagnostics retain source jumps. `r` repeats the selected
command, `s` stops it, and `<Space>Cq` or `:cargo cancel` also cancels it.
There is one Cargo process at a time. Commands receive closed stdin; use the
integrated terminal for interactive programs. Completed commands immediately
reload clean files changed on disk, including changes made by `cargo fmt`.
Only check commands enable watching saves.

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
Lens-style messages beside affected source lines in Normal mode; manual
completion; a wrapped, scrollable hover box on `K`; versioned inlay hints
at their UTF-16 positions in Normal mode (toggle with `<Space>uh`);
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
| GPU desktop frontend | Optional egui/wgpu Vulkan window with native explorer/tabs/toolbars, DPI scaling, zoom, modal input, clipboard/IME routing, pointer selection, and shared Git/Cargo/LSP/PTY runtime. Offscreen Vulkan screenshots and focused input/lifecycle tests exist; interactive Wayland/X11 validation remains release work. |
| UTF-8 buffer, grapheme edits, transactions, branching undo, atomic/conflict-aware saves | Implemented with regression tests. Undo stores byte-range changes with entry and byte limits; grapheme indexes are cached per changed line. The line-vector text model still needs broader large-file performance validation. |
| Modal grammar, registers/macros, search, Visual modes, panes, buffers, leader registry | Usable MVP subset with known deviations documented in `docs/COMMANDS.md`. |
| Terminal frontend | Raw mode, bracketed paste, resize events, local syntax colors, gutters/status/messages (including modified, large-file, and loaded read-only flags), split/explorer/picker/leader drawing, horizontal cursor tracking, Unicode cell widths, changed-cell output, and a toggled PTY-backed terminal panel are implemented. Mouse forwarding, terminal selection/copy, and suspend/resume remain. |
| Project discovery, finder, and text/regex search | Root selection is a synchronous ancestor walk before the first frame. Full-tree scanning and text search then run in background threads, stream through bounded cancellable queues, and honor hidden/ignored controls. The interactive grep picker uses regex mode and cancels the previous task when its query changes; previews are not yet rendered. Opening the file finder rescans the project in the background (at most every two seconds) without clearing its index, adding new files and dropping removed ones; saved files are added immediately. |
| Explorer | Expandable directory tree with lazy background loading, keyboard navigation, active-file reveal, hidden/ignored filters, and session-persisted expansion. A save re-lists its directory at once, and cached directories whose modification time changes, for example after a file is created in the terminal, are re-listed within about two seconds while idle. Confirmed create/rename/delete operations remain. |
| `rust-analyzer` | Supervised asynchronous transport, initialization, manual restart, versioned full-text sync, typed requests, diagnostics, hover boxes, inlay rendering, format-on-save, basic response UI/edit application, size bounds, and failure status are wired. Snippets, code-action execution, multi-file rename, general request cancellation, and capability-specific disabled states remain. |
| Cargo panel | Common Cargo commands, bounded live output, cancellation, explicit and save-triggered checks, JSON diagnostics, and source jumps are implemented with process-level tests. Runs share cargo's build-directory lock with rust-analyzer's own check-on-save, so one can briefly wait for the other. |
| Git | Background status, branch/file counts, staged/unstaged active-file gutters, hunk navigation, colored diff snapshots, current-line commit metadata/patches, and explicit file staging/unstaging have repository and PTY tests. Git operations use saved files; hunk staging, conflict resolution, and commit creation remain terminal workflows. |
| Motion | Optional time-based scrolling and cursor animation use the custom cell renderer without changing logical positions. Deterministic timing tests cover settling, retargeting, resizing, reduced motion, and Unicode cells. Pixel motion is outside a cell terminal's capabilities. |
| Recovery and sessions | Dirty named and scratch/stdin buffers are journaled asynchronously, queued journal work is joined at shutdown, and content-free named-file sessions restore active selection, pane cursor/viewports, and explorer metadata. Recovery choice/application, unnamed session buffers, exact split topology/orientation, explicit-discard cleanup, and persistent undo are not release-complete. |
| Performance/release evidence | `editor-bench` reports warm 1 MiB buffer-open and component p95 checks for insertion, long-word movement, whole-file indentation, long-line rendering, and finder input. It does not measure process launch through terminal flush or runtime maintenance, has no target-laptop/CI baseline yet, and therefore does not prove the complete 50 ms/8 ms release budgets. Broader fuzz/property coverage remains release work. |

For module ownership and boundaries, see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The dependency/native-boundary
review required by Design Requirement 1 is in
[docs/DEPENDENCIES.md](docs/DEPENDENCIES.md).

## License

Licensed under the [MIT License](LICENSE).
