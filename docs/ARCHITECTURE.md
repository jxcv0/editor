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
| `app` | Foreground event loop, rendering cadence, terminal-process polling, background scan/search polling, LSP and `codex-watch` dispatch, diagnostics, clean-file reloads, journaling, session I/O, and health output | Owns process/integration handles and is the only layer that converts `EditorRequest` values or worker events into UI/core actions. Worker drains are bounded. Periodic clean-file checks use synchronous metadata only when unchanged, but changed files are still reread on the foreground thread; config reload is synchronous too. |
| `buffer` | UTF-8 text storage, grapheme positions, byte and UTF-16 conversion, edit transactions, branching undo/redo, loading, conflict detection, and atomic saving | Owns text and disk identity. It has no terminal, project-search, or tool-process knowledge. The current representation is line-based strings, not a rope/piece table, so the largest size/performance goals still need measurement and likely further work. |
| `editor` | Modal state machine, panes/layout, per-pane cursor/viewport, buffers, registers/macros, selection, prompts, picker/explorer state, diagnostics, and typed integration requests | Consumes terminal-neutral `Key` values and mutates `Buffer`s. It emits `EditorRequest` values rather than launching tools. Vim compatibility is intentionally bounded; see [COMMANDS.md](COMMANDS.md). |
| `input` | Small terminal-neutral key vocabulary | Keeps Crossterm types out of the editor state machine and leaves room for another frontend. |
| `ui` | Crossterm terminal lifecycle, input translation, cell canvas, Unicode display width, and changed-cell rendering | The only terminal-specific module. Its RAII guard restores raw mode, cursor, bracketed paste, and alternate screen on ordinary drop; a panic hook performs emergency restoration. Suspend/resume behavior and deterministic full-frame snapshots are not yet complete. |
| `command` | Declarative typed command IDs and built-in leader hierarchy | Shared vocabulary for editor, `rust-analyzer`, and `codex-watch` actions. The registry describes commands; context availability and actual execution remain the orchestrator/editor's responsibility. |
| `config` | Versioned TOML schema, defaults, user/project layer merge, validation, and project-tool trust filtering | Contains no evaluation hook. Keymap values can name typed commands but are not applied. `:reloadconfig` replaces valid typed settings, while existing explorer/undo state and integration workers are not reconstructed. |
| `project` | Project-root discovery, ignored/hidden-aware walking, fuzzy ranking, and literal/regex project search | The short ancestor-based root discovery is synchronous during composition. Full scans/searches run on cancellable background threads and stream through bounded channels. The module has no editor mutation access; preview/open behavior is handled above it. |
| `syntax` | Lightweight, line-local Rust/TOML/Markdown highlighting | Always-available fallback with no parser process. It is lexical and deliberately tolerant, not a full incremental syntax tree or semantic highlighter. |
| `terminal` | PTY shell lifecycle, bounded asynchronous output, VT screen/scrollback state, resizing, terminal-key encoding, and bounded capability replies | The editor owns only the renderable emulator state and emits typed input/toggle requests; `app` owns the fallible OS process handle. The interactive shell starts only after an explicit terminal command. |
| `bin/editor-bench` | Repeatable local smoke measurement for warm 1 MiB open and edit-plus-frame p95 | Measures useful core proxies, not full process-launch-to-terminal-flush latency. Target-laptop baselines and regression enforcement are still needed. |
| `process` | Direct child spawning, bounded stdin/stdout/stderr, process-group shutdown, bounded logs, and line framing helpers | Security/reliability boundary shared by integrations. It never invokes a shell. On Unix it creates a child process group; non-Unix shutdown falls back to the platform process API. |
| `lsp` | Asynchronous `rust-analyzer` lifecycle and JSON-RPC/LSP transport, document snapshots/version checks, generic request/notification routing, and bounded events/errors | Runs process and protocol work on a dedicated worker. `app` maps typed actions to methods and handles a practical response subset; comprehensive capability-aware UX remains incomplete. |
| `codex_watch` | Explicit enablement/run-mode state machine, `codex-watch --json-events` supervision, bounded event parsing/logs, and start/stop/restart/run-once operations | Runs on a dedicated worker and refuses to start without enablement plus a selected dry-run/workspace-write mode. Applying filesystem changes safely remains an editor/orchestrator concern. |
| `state` | Private asynchronous recovery journal and content-free versioned session files | During periodic maintenance, `app` journals dirty named and scratch/stdin buffers, queues removal after observing them clean, reports available recovery records, joins queued journal work at shutdown, and saves/loads named-file session metadata. Active selection, pane cursors/viewports, and explorer state are restored; recovery selection/application, unnamed session buffers, exact split topology/orientation, and persistent undo remain MVP work. |

## Runtime ownership and data flow

The foreground `app::Runtime` thread owns `Editor`, terminal input, and
rendering. It performs small, nonblocking polls of worker handles between input
events. Idle ticks continue polling services but render only after input or a
worker result changes visible state. `ui::FrameBuilder` retains line syntax
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
                                  |          /    |      |       \
                                  v     terminal project lsp   codex_watch
                           ui::Renderer        |        |        |
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
metadata polling are exceptions to the broader nonblocking-I/O goal; a detected
change also causes a foreground reread. These paths still need to move or be
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
  the entire growing project. Regex compilation also runs in the search worker.
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

`RustAnalyzerClient` and `CodexWatch` each add a protocol/state worker above
that primitive. Both expose nonblocking command and event APIs. They are
optional: construction/start failures become status/messages while the local
buffer and terminal editor continue to work. `app` starts `rust-analyzer` only
after rendering the first frame and routes explicit restart requests to its
worker; `codex-watch` never starts until a leader command selects a run mode
and requests execution.

The integrated terminal is a separate, explicitly interactive boundary. It
intentionally starts the user's `$SHELL` directly in a PTY after `<Space>t` or
`:terminal`, never as an implicit implementation detail of another command.
The PTY reader uses a bounded queue, the VT model keeps bounded scrollback,
terminal capability responses are queued with a fixed limit, and dropping the
runtime closes the PTY and terminates the owned shell.

There is deliberately no dynamic library loading, embedded language, arbitrary
plugin callback, or general task runner.

## Honest MVP status

The repository contains meaningful tested building blocks, but `DESIGN.md`
defines a larger release bar than the present implementation. In particular,
do not infer end-to-end support merely from a command ID or public client
method. The following areas require continued integration and verification:

- complete edge-case behavior for every documented modal, picker, explorer,
  LSP, `codex-watch`, recovery, and session path;
- safe UI flows for external file changes and dirty-buffer conflicts;
- complete completion/signature/snippet, navigation, rename, code-action,
  formatting, diagnostics, and inlay-hint UX above the generic LSP transport;
- lazy hierarchical explorer mutation/confirmation flows and automatic
  external-filesystem refresh;
- system clipboard providers and persistent undo;
- large-file degradation policy up to the stated limits;
- config reload/provenance and health-report completeness;
- performance benchmarks proving the 50 ms startup and 8 ms input-to-frame
  p95 budgets on the target laptop;
- fuzz/property coverage and terminal snapshot/restore coverage required for
  release.

Until those criteria pass, this should be described as an MVP implementation
in progress, not as a complete release satisfying all 23 design requirements.
