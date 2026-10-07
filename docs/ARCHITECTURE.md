# Architecture

This editor is a single foreground Rust process with a terminal-independent
editing core. Optional tools are child processes, not linked libraries or
plugins. The key design boundary is that terminal I/O, filesystem traversal,
and protocol parsing may produce events for the core, but they do not own or
mutate editor text directly.

## Module map

| Module | Responsibility | Boundary and current scope |
| --- | --- | --- |
| `main` | CLI action handling, project selection, configuration load, initial buffers/cursor, and handoff to the runtime | This is the thin executable composition root. It performs no terminal drawing or tool-protocol work itself. |
| `cli` | Typed parsing of paths, stdin, one-based startup positions, `--no-session`, help/version, and `--` | Preserves paths as `OsString`/`PathBuf` and deliberately leaves filesystem classification, project selection, and all side effects to `main`. |
| `app` | Foreground event loop, rendering cadence, terminal-process polling, background scan/search polling, LSP dispatch, diagnostics, clean-file reloads, journaling, session I/O, and health output | Owns process/integration handles and is the only layer that converts `EditorRequest` values or worker events into UI/core actions. Worker drains are bounded. Periodic clean-file checks use synchronous metadata only when unchanged, but changed files are still reread on the foreground thread; config reload is synchronous too. |
| `buffer` | UTF-8 text storage, grapheme positions, byte and UTF-16 conversion, edit transactions, branching undo/redo, loading, conflict detection, and atomic saving | Owns text and disk identity. It has no terminal, project-search, or tool-process knowledge. The current representation is line-based strings, not a rope/piece table, so the largest size/performance goals still need measurement and likely further work. |
| `editor` | Modal state machine, panes/layout, per-pane cursor/viewport, buffers, registers/macros, selection, prompts, picker/explorer state, diagnostics, and typed integration requests | Consumes terminal-neutral `Key` values and mutates `Buffer`s. It emits `EditorRequest` values rather than launching tools. Vim compatibility is intentionally bounded; see [COMMANDS.md](COMMANDS.md). |
| `explorer` | Cached directory listings, visible tree rows, expansion, selection, and active-file reveal | Performs no filesystem I/O. `app` supplies bounded batches from one lazy directory scan at a time, and replaces a cached listing with a complete fresh one after a save in that directory or a change of its modification time; the file finder retains its independent recursive index. |
| `input` | Small terminal-neutral key vocabulary | Keeps Crossterm types out of the editor state machine and leaves room for another frontend. |
| `ui` | Crossterm terminal lifecycle, input translation, cell canvas, Unicode display width, and changed-cell rendering | The only terminal-specific module. Its RAII guard restores raw mode, cursor, bracketed paste, and alternate screen on ordinary drop; a panic hook performs emergency restoration. Suspend/resume behavior and deterministic full-frame snapshots are not yet complete. |
| `command` | Declarative typed command IDs and built-in leader hierarchy | Shared vocabulary for editor and `rust-analyzer` actions. The registry describes commands; context availability and actual execution remain the orchestrator/editor's responsibility. |
| `config` | Versioned TOML schema, defaults, user/project layer merge, validation, and project-tool trust filtering | Contains no evaluation hook. Keymap values can name typed commands but are not applied. `:reloadconfig` replaces valid typed settings, while existing explorer/undo state and integration workers are not reconstructed. |
| `project` | Project-root discovery, ignored/hidden-aware walking, fuzzy ranking, and literal/regex project search | The short ancestor-based root discovery is synchronous during composition. Full scans/searches run on cancellable background threads and stream through bounded channels. The module has no editor mutation access; preview/open behavior is handled above it. |
| `syntax` | Lightweight, line-local Rust/TOML/Markdown highlighting | Always-available fallback with no parser process. It is lexical and deliberately tolerant, not a full incremental syntax tree or semantic highlighter. |
| `terminal` | PTY shell lifecycle, bounded asynchronous output, VT screen/scrollback state, resizing, terminal-key encoding, and bounded capability replies | The editor owns only the renderable emulator state and emits typed input/toggle requests; `app` owns the fallible OS process handle. The interactive shell starts only after an explicit terminal command. |
| `check` | Common Cargo process runs, program output, JSON diagnostic parsing, path resolution, and panel state | A worker parses and streams bounded entries; the editor owns the renderable state while `app` owns the cancellable task. Commands start explicitly; check alone can also rerun after watched saves. |
| `git` | Saved-file repository status, literal paths, unified diffs/hunks, staged-to-worktree coordinate mapping, line blame/commit views, and explicit file staging | A bounded background worker owns supervised Git children. The editor owns the status/diff presentation; runtime rejects stale line data. Each child has a timeout and output cap. Index writes require explicit commands and never save buffers. |
| `animation` | Time-based, bounded presentation interpolation | No buffer or terminal ownership. `ui::FrameBuilder` retains per-pane motion separately from logical cursors/viewports. Deterministic timestamp-driven tests cover retargeting and settling. |
| `bin/editor-bench` | Repeatable local smoke measurement for warm 1 MiB open and edit-plus-frame p95 | Measures useful core proxies, not full process-launch-to-terminal-flush latency. Target-laptop baselines and regression enforcement are still needed. |
| `process` | Direct child spawning, bounded stdin/stdout/stderr, process-group shutdown, and bounded logs | Security/reliability boundary shared by integrations. It never invokes a shell. On Unix it creates a child process group; non-Unix shutdown falls back to the platform process API. |
| `lsp` | Asynchronous `rust-analyzer` lifecycle and JSON-RPC/LSP transport, document snapshots/version checks, generic request/notification routing, and bounded events/errors | Runs process and protocol work on a dedicated worker. `app` maps typed actions to methods and handles a practical response subset; comprehensive capability-aware UX remains incomplete. |
| `state` | Private asynchronous recovery journal and content-free versioned session files | During periodic maintenance, `app` journals dirty named and scratch/stdin buffers, queues removal after observing them clean, reports available recovery records, joins queued journal work at shutdown, and saves/loads named-file session metadata. Active selection, pane cursors/viewports, and explorer state are restored; recovery selection/application, unnamed session buffers, exact split topology/orientation, and persistent undo remain MVP work. |

## Optional desktop frontend

The `gui` Cargo feature compiles `gui::{font,graphics,input,view}`. The same `editor`
binary selects the native frontend with `--gui`; default builds do not link
egui, winit, or wgpu. Startup composition and file/session arguments are shared.

`gui::App` owns the existing `app::Runtime` on the winit foreground thread.
Native events become the same `InputEvent`/`Key` values as terminal events.
It polls integrations every 25 ms without blocking input, begins background
services after the first presented frame, and requests frames only for input,
worker changes, or animation/repaint deadlines. It never enters Crossterm raw
mode. Normal exit saves session metadata and dropping the runtime drains the
journal and tears down its child processes.

`gui::graphics` owns the Vulkan surface/device, egui input adapter, texture
uploads, render pass, resize/scale changes, and surface-loss retries. Device
errors end the frontend with an error instead of silently losing the window.
Zero-sized windows skip presentation. `gui::view` owns native chrome, font size,
pointer/clipboard presentation, and the unsaved-close dialog. The resizable
explorer and tabs use core state; toolbar commands finish open transactions
before dispatching ordinary typed or Ex actions. Window close checks every
buffer, including hidden dirty buffers.

The shared `ui::FrameBuilder` composes source, syntax, inlays, diagnostics,
split panes, pickers, Git, and terminal cells. The desktop omits the terminal
explorer and replaces the status row with native chrome, then paints the cells
as egui glyphs and merged background strips. It does not run the TUI in a child
process. Hit testing uses the presented pane geometry, tab stops, Unicode
graphemes, and inlay widths. Motion retains its cell coordinate model.

`gui::font` embeds FiraCode Nerd Font Mono as the default font for both egui
families, retaining bundled fallback fonts. Identically styled ASCII cells
form runs for the font's `calt` programming ligatures. The evaluator handles
the pinned font's single substitutions and chained contexts; a reachability
test audits its supported lookup forms and one-cell advances. It is not a
general Unicode shaper. Unicode graphemes use the ordinary egui path, and
style/row boundaries terminate runs. Shaping changes only glyphs, never buffer
text, canvas widths, cursor positions, or hit testing. Up to 1,024 shaped runs
are cached; substituted glyphs share a bounded 1,024-square atlas, rebuilt on
font-size/DPI changes. If it fills, affected runs use ordinary glyphs. The font
asset, version, checksum, and license are recorded in `assets/fonts/README.md`.

Git `Document`s include original unified text and worker-prepared `DiffRow`s.
The parser tracks old/new hunk counts, pairs contiguous removals/additions in
linear time, and keeps context aligned with blank cells for unequal blocks.
Both frontends reuse the same split renderer, with a unified fallback below
64 columns. Binary/rename/commit metadata stays readable. No similarity search
or per-frame patch parsing is performed.

GUI unit tests cover input translation, modal clipboard selection, window-close
protection, conflict-aware toolbar saves, Unicode pointer mapping, and egui
tessellation at multiple sizes/scales. The ignored `offscreen_desktop_and_diff_render`
test exercises wgpu with a real Vulkan adapter (including Mesa software Vulkan),
checks for GPU validation errors, and saves screenshots. Interactive Wayland/X11
input, clipboard, and IME still require desktop validation.

## Runtime ownership and data flow

The foreground `app::Runtime` thread owns `Editor`, terminal input, and
rendering. It performs small, nonblocking polls of worker handles between input
events. Idle ticks continue polling services but render only after input, a
worker result, or an active animation requires a frame. Motion uses a 16 ms
poll interval until settled, then restores the ordinary 25 ms service poll.
`ui::FrameBuilder` retains line syntax
and display checkpoints using buffer-provided line identities. Lexical work
is capped at 16 KiB per line; the remainder remains readable as plain text.
Project traversal and child-process work are isolated behind bounded queues:

```text
terminal event -> input::Key -> editor::Editor -> buffer::Buffer
                                  |       |
                                  |       +-> editor::EditorRequest
                                  |                   |
                                  v                   v
                              ui::Canvas        app::Runtime
                                  |          /        |         \
                                  v    terminal    project      lsp
                           ui::Renderer    |          |          |
                                           +--- bounded events --+

buffer changes -> state::Journal (queued snapshot)
clean exit     -> state::SessionState (paths/layout only)
```

The ownership invariant is one-way: workers return data or status, and only the
foreground editor may change UI state or text. Mutating format/rename responses
are checked against their originating active path/version, and versioned stale
diagnostics are discarded; cancellation and staleness handling for the rest of
the response UI remain incomplete. Workers do not hold the interactive path
waiting on I/O. The current foreground config reload and two-second clean-file
and listed-directory metadata polling are exceptions to the broader
nonblocking-I/O goal; a detected file change also causes a foreground reread,
while a changed directory is listed again on a background worker. These paths still need to move or be
measured.

## Core invariants

- A `Buffer` is the sole owner of its text, disk snapshot, version, and undo
  tree. Every committed change whose final text differs advances its monotonic
  version; a transaction that returns exactly to its starting text is a no-op.
- A `Pane` references a buffer by index but owns its own cursor, selection
  anchor, desired column, and viewport. Multiple panes can therefore share one
  text/undo history without sharing navigation state.
- Positions use explicit `Pos` (line plus grapheme), `ByteOffset`, and
  `Utf16Pos` types. Conversions reject invalid boundaries.
- One insert session or explicit operator/edit transaction is intended to
  produce one undo node. Nodes retain byte-range changes, preserving exact
  separators and byte boundaries even when edits join grapheme clusters.
  Redo branches are retained within configurable entry and change-storage byte
  bounds. An oversized committed change clears history; active transactions
  retain rollback data until they end. Tree metadata is bounded by entry count.
- A normal save checks the captured on-disk identity before replacement,
  preserves line separators/final-newline state, follows an existing symlink
  target, and preserves Unix mode bits where supported. Force-save is a
  separate API.
- External-process input is untrusted and bounded. Protocol stdout uses
  cancellable backpressure instead of discarding bytes; stderr logs may drop
  events. A separate bounded stdin writer preserves FIFO order without
  preventing the integration worker from draining stdout or handling stop.
- Project scans/searches are cancellable and result-bounded. Starting a new UI
  query cancels stale work. The file-finder worker owns normalized relative
  paths and reuses matching scratch storage; only current query/index results
  reach the picker. Scan batches append to the index instead of republishing
  the entire growing project, and repeated paths are ignored. A rescan marks
  the paths it sees and sweeps the rest only after a complete walk, so the
  index stays searchable while it refreshes. Regex compilation also runs in the search worker.
- Persisted state and recovery formats are versioned. Session data contains
  paths/layout metadata, never buffer contents; recovery records hold dirty
  text separately.

## Configuration boundary

Configuration precedence in the library is:

1. Built-in defaults.
2. `$XDG_CONFIG_HOME/editor/config.toml`, or the platform config-directory
   fallback returned by `dirs`.
3. `<project-root>/.editor.toml`.

Project `tools` settings are removed unless the caller marks the project as
trusted. The schema rejects unknown fields and versions other than `1`.
Command-line overrides, trust persistence, keymap application, provenance per
field, and full effective-config display are design goals, not completed
behaviors. `:reloadconfig` atomically replaces the editor's settings only after
a valid load. Values consulted on later operations can take effect immediately,
but it does not rebuild existing state or process integrations; construction-
time tool paths, arguments, and process/protocol limits require a restart.

## Process integration boundary

`ProcessSpec` retains the executable, argument vector, working directory, and
explicit environment overrides as structured values. `SupervisedChild` starts
it directly and owns piped stdio plus a bounded shutdown interval. This avoids
shell parsing and keeps child lifetime management reusable.

`RustAnalyzerClient` adds a protocol/state worker above that primitive and
exposes nonblocking command and event APIs. It is optional: construction/start
failures become status/messages while the local buffer and terminal editor
continue to work. `app` starts `rust-analyzer` only after rendering the first
frame and routes explicit restart requests to its worker.

The integrated terminal is a separate, explicitly interactive boundary. It
intentionally starts the user's `$SHELL` directly in a PTY after Ctrl-backtick or
`:terminal`, never as an implicit implementation detail of another command.
The PTY reader uses a bounded queue, the VT model keeps bounded scrollback,
terminal capability responses are queued with a fixed limit, and dropping the
runtime closes the PTY and terminates the owned shell.

Cargo commands are another explicit boundary. `<Space>C` and `:cargo` select
from typed common subcommands. The existing check worker also retains bounded,
live program output for run/test and cancels the process group when replaced
or stopped. It never starts a shell or supplies interactive stdin.
`<Space>cc`, `:check`, or a
save of a Rust source or Cargo manifest while the watch panel is visible starts
`cargo check --message-format=json` directly through `SupervisedChild`. A
worker thread parses stdout into bounded entries and classifies stderr status
lines; at most one run exists, and starting another, hiding the panel, or
dropping the runtime kills the previous run's process group.

Format-on-save carries a pending save intent with its document revision and
originating pane. The runtime applies valid formatting edits in one undo
transaction, performs the conflict-aware write, sends didSave, and only then
completes a requested pane close. Stale intents cannot write; unavailable or
timed-out formatting falls back to a normal save. Inlay responses carry the
same document revision checks, and hover responses also check the input
generation. The UI draws hints and diagnostic text only in Normal mode and
keeps hover boxes within the active pane.

There is deliberately no dynamic library loading, embedded language, arbitrary
plugin callback, or general task runner.

Git status starts after the first frame and refreshes on active-file changes,
saves, and idle intervals. One worker serializes bounded subprocess calls,
with at most one explicit request waiting behind it. Paths retain OS-string
identity through NUL-separated status parsing and literal argument vectors.
Diff/textconv helpers are disabled. Separate zero-context staged/unstaged
hunks drive the gutters; binary searches map staged index coordinates through
unstaged hunks without scanning every hunk per visible line. Markers are
accepted only for clean matching revisions whose disk metadata still matches.
View generations prevent dismissed background requests reopening overlays.
Whole-file staging and unstaging never modify buffer text; workers join at
shutdown after cancellation. Repository state can change externally, so Git
views remain refreshable snapshots rather than transactions across Git calls.

The terminal frontend already uses a custom changed-cell renderer. Animation
adds per-pane presentation state without introducing a TUI or graphics
dependency. Normal-mode viewport/cursor transitions ease for 100/70 ms;
logical navigation updates immediately. Large scroll travel is bounded to
half a pane, and edits, resize, overlays, and Insert mode reset motion.
`draw_editor` stays deterministic for benchmarks/tests; the runtime uses
`draw_animated_at` and keeps rendering until all motion settles. No animation
state is written to session metadata. Independent configuration and a leader
toggle provide reduced motion.

## Honest MVP status

The repository contains meaningful tested building blocks, but `DESIGN.md`
defines a larger release bar than the present implementation. In particular,
do not infer end-to-end support merely from a command ID or public client
method. The following areas require continued integration and verification:

- complete edge-case behavior for every documented modal, picker, explorer,
  LSP, recovery, and session path;
- safe UI flows for external file changes and dirty-buffer conflicts;
- complete completion/signature/snippet, navigation, rename, code-action,
  formatting, diagnostics, and inlay-hint UX above the generic LSP transport;
- explorer mutation/confirmation flows, and external-filesystem refresh
  without polling;
- system clipboard providers and persistent undo;
- large-file degradation policy up to the stated limits;
- config reload/provenance and health-report completeness;
- performance benchmarks proving the 50 ms startup and 8 ms input-to-frame
  p95 budgets on the target laptop;
- fuzz/property coverage and terminal snapshot/restore coverage required for
  release.

Until those criteria pass, this should be described as an MVP implementation
in progress, not as a complete release satisfying all 23 design requirements.
