# Design

## Requirement 1 — Implementation and dependency boundary

All first-party editor code shall be written in Rust. The editor shall not
embed a scripting language or inherit a Vim/Neovim runtime.

Pure-Rust dependencies are preferred. A dependency containing or linking
native C/C++ code may be adopted only after assessing:

- Its purpose and pure-Rust alternatives
- Performance and startup impact
- Portability and build complexity
- Security and maintenance risk
- Licensing

`rust-analyzer` and `codex-watch` are the initially required external tool
integrations, not linked implementation dependencies. External tools shall be
connected through a small, reusable process and protocol abstraction so that
future integrations can be added without changing the editing core or
introducing a general-purpose plugin runtime.

## Requirement 2 — Performance budgets

For a release build opening a local UTF-8 file up to 1 MiB on the target
laptop:

- Time from process launch to an editable first frame shall be at most 50 ms at
  p95 with a warm filesystem cache.
- Input handling and production of the resulting frame shall take at most 8 ms
  at p95.
- Project discovery, `rust-analyzer`, `codex-watch`, and other external tools
  shall initialize asynchronously and never delay basic editing.
- Repeatable benchmarks shall track these budgets and report regressions.

## Requirement 3 — Modal editing contract

The editor shall implement a documented, Vim-compatible editing language
without attempting complete Vim emulation.

- Core modes: Normal, Insert, Visual character, Visual line, Visual block, and
  Operator-pending.
- Counts, operators, motions, registers, and text objects shall compose as they
  do in Vim—for example, `2dw`, `d2w`, and `ciw`.
- Supported commands shall have deterministic behavioral tests.
- Intentional deviations from Vim shall be documented.
- Unsupported commands shall provide clear feedback and never silently perform
  a different edit.

## Requirement 4 — Initial editing command set

The first usable release shall support, at minimum:

- Motions: `h j k l`, `w b e`, `0 ^ $`, `gg G`, `%`, `f F t T`, `; ,`, and
  `Ctrl-d/u/f/b`
- Insert entry: `i I a A o O`, with `Esc` returning to Normal mode
- Operators: `d c y`, composable with counts, supported motions, and repeated
  line forms such as `dd`
- Text objects: words, quoted strings, and paired `()`, `[]`, and `{}`
  delimiters
- Direct edits: `x`, `r`, `J`, `p`, and `P`
- History: `u`, `Ctrl-r`, and repeat-last-change `.`
- Search: `/`, `?`, `n`, `N`, `*`, and `#`
- Visual character, line, and block selections using the same applicable
  operators
- Basic commands: `:w`, `:q`, `:q!`, `:wq`, and `:e`
- The unnamed register plus named registers `a`–`z`

## Requirement 5 — `rust-analyzer` integration

For each detected Cargo workspace, the editor shall asynchronously launch and
supervise one configurable `rust-analyzer` process using LSP over stdio.

The initial integration shall provide:

- Versioned incremental document synchronization
- Diagnostics without disrupting input
- Completion and signature help
- Hover information
- Go to definition, type definition, and implementation
- Find references
- Symbol rename
- Code actions
- Formatting
- Inlay hints

If `rust-analyzer` is missing, initializing, unresponsive, or crashes, editing
shall remain fully available. The editor shall expose its status and errors and
provide a manual restart action. It shall never download or update
`rust-analyzer` automatically.

## Requirement 6 — Discoverable `<Space>` menu

In Normal and Visual modes, `<Space>` shall enter a leader-menu state similar
to LazyVim's `which-key` experience.

- A keyboard-driven overlay shall show valid next keys, descriptions, and
  nested command groups.
- Commands from the editor, `rust-analyzer`, and `codex-watch` shall share one
  declarative command registry.
- Only supported commands shall appear; temporarily unavailable actions shall
  be visibly disabled with a reason.
- The menu shall close after executing a command or pressing `Esc`.
- An invalid sequence shall make no edit and provide clear feedback.
- Default categories should retain familiar LazyVim conventions where
  relevant, such as files, buffers, code, search, and diagnostics.
- Menu bindings shall be configurable without requiring a scripting runtime.

## Requirement 7 — `codex-watch` integration

The editor shall supervise one `codex-watch --json-events` process per active
Git project.

- Starting it shall require explicit per-project enablement, which may be
  remembered.
- The UI shall show whether it is stopped, watching, processing, completed, or
  failed.
- Users shall be able to start, stop, restart, run once, and inspect captured
  output.
- Dry-run and workspace-write modes shall require explicit selection and remain
  visibly indicated.
- Task lifecycle events shall appear without blocking editing.
- Changes to clean open buffers shall reload safely while preserving cursor
  position where possible.
- Changes conflicting with unsaved buffers shall never overwrite them silently;
  the editor shall offer review or reload choices.
- Missing binaries, malformed events, and crashes shall be reported without
  affecting normal editing.

## Requirement 8 — Initial platform and terminal UI

The initial product shall be a terminal editor optimized for this laptop's
Linux environment.

- It shall run as one foreground process with no GUI, browser runtime, or
  required daemon.
- It shall support UTF-8 text, grapheme-aware cursor movement, correct terminal
  column widths, true color with a reasonable fallback, terminal resizing, and
  bracketed paste.
- Rendering shall update only changed terminal cells to minimize latency and
  flicker.
- Editing shall be fully keyboard-accessible; mouse support is optional.
- Terminal state shall be restored after normal exits, recoverable failures,
  panics, and suspend/resume.
- Portability beyond Linux is desirable but not an initial release requirement.
- The editing core shall remain separated from terminal-specific input and
  rendering so another frontend could be added later without rewriting it.

## Requirement 9 — Toggleable project file browser

The TUI shall provide a docked project-tree panel on the left, similar to
LazyVim's explorer.

- `<Space>e` shall open and focus it, focus it when already open, and close it
  when invoked from the focused explorer.
- Closing it shall return its full width to the editor.
- Its root shall follow the active Cargo workspace, then the nearest Git root,
  falling back to the working directory.
- It shall reveal and highlight the active file.
- Directories shall load lazily and asynchronously.
- Keyboard navigation shall support `j/k`, `h/l`, `Enter`, and `Esc`, with
  familiar actions for creating, renaming, and deleting files.
- Destructive actions shall require confirmation, and no operation may silently
  discard an unsaved buffer.
- Hidden and ignored files shall be independently toggleable.
- External filesystem changes shall update the tree without blocking editing.
- Panel width and expanded directories shall be remembered for the session.

## Requirement 10 — Buffers and split panes

The editor shall support multiple open buffers displayed in arbitrary
horizontal and vertical split panes.

- A buffer displayed in multiple panes shall share text and undo history while
  each pane retains its own cursor, selection, and viewport.
- `<Space>bb` shall open a fuzzy buffer switcher and `<Space>bd` shall close the
  current buffer.
- `<Space>-` and `<Space>|` shall create horizontal and vertical splits.
- `Ctrl-h/j/k/l` shall move focus between panes.
- Standard commands shall include `:b`, `:bn`, `:bp`, `:bd`, `:split`,
  `:vsplit`, and `:only`.
- Closing a pane shall not implicitly discard its buffer.
- Closing a modified buffer or quitting shall require saving, explicit discard,
  or a forced command.
- Layout changes and terminal resizing shall preserve each pane's cursor and
  viewport where possible.
- An always-visible buffer tab bar and Vim-style tab pages are not required
  initially; the buffer switcher and panes form the primary navigation model.

## Requirement 11 — Project finding and search

The editor shall provide built-in, asynchronous project navigation without
requiring `fzf`, `ripgrep`, or another external search process.

- `<Space><Space>` and `<Space>ff` shall open a fuzzy project-file finder.
- `<Space>sg` shall provide streaming project-wide text and regular-expression
  search.
- `<Space>ss` and `<Space>sS` shall search document and workspace symbols
  through `rust-analyzer`.
- Results shall include a preview and support opening in the current pane or a
  new split.
- Filesystem search shall respect `.gitignore` by default, with controls to
  include hidden or ignored files.
- Results shall appear incrementally rather than waiting for a complete scan.
- Changing a query shall cancel stale work promptly.
- Scanning and ranking shall use bounded background work and never block input
  or rendering.
- The implementation should use pure-Rust libraries and require no persistent
  index.

## Requirement 12 — File integrity and crash recovery

The editor shall prioritize preventing silent source loss or corruption.

- UTF-8 shall be the initial supported encoding; invalid UTF-8 shall never be
  replaced or transcoded silently.
- Existing line-ending style, final-newline state, file permissions, and
  symbolic links shall be preserved when saving.
- Saves shall use an atomic replacement strategy when supported by the
  filesystem.
- Before saving, the editor shall detect whether the on-disk file changed since
  it was loaded or last saved.
- A dirty buffer shall never be overwritten by an external change; the user
  shall be offered diff, reload, or explicit overwrite actions.
- Modified buffers shall be journaled asynchronously to a private recovery
  location without delaying input.
- After a crash or interrupted session, recoverable buffers shall be listed on
  the next launch.
- Recovery data shall be removed once its buffer is safely saved or explicitly
  discarded.

## Requirement 13 — Syntax and diagnostic presentation

Rust code shall remain readable immediately after opening, without waiting for
`rust-analyzer`.

- Rust, TOML, and Markdown shall receive local syntax highlighting; unknown
  formats shall remain usable as plain text.
- Highlighting shall update incrementally and degrade gracefully on incomplete
  or invalid syntax.
- When available, `rust-analyzer` semantic information may refine local
  highlighting without replacing it.
- Stale analyzer responses shall be discarded using document versions.
- Diagnostics shall use severity-colored gutter markers and text decoration
  without modifying buffer contents.
- `[d` and `]d` shall navigate diagnostics; `gl` shall show details for the
  current line.
- `<Space>xX` shall list current-buffer diagnostics and `<Space>xx` shall list
  workspace diagnostics.
- Inlay hints shall be toggleable with `<Space>uh`.
- Diagnostic, semantic, and inlay-hint updates shall preserve the logical
  cursor, selection, and viewport.
- Highlighting technology and any native parser dependency shall undergo the
  dependency assessment required by Requirement 1.

## Requirement 14 — Completion and Rust code actions

When `rust-analyzer` is ready, Insert mode shall provide asynchronous completion
without delaying typing.

- Completion shall open automatically in suitable contexts and manually with
  `Ctrl-Space`.
- `Ctrl-n/p` shall change the selection, `Enter` shall accept it, and `Ctrl-e`
  shall dismiss it.
- No completion shall alter text until explicitly accepted.
- The popup shall show the candidate's kind and provide documentation without
  obscuring the current line.
- LSP snippets shall support placeholder navigation with `Tab` and `Shift-Tab`.
- Signature help shall appear contextually and remain manually dismissible.
- Navigation defaults shall include `K` for hover, `gd` for definition, `gD`
  for declaration, `gy` for type definition, `gI` for implementation, and `gr`
  for references.
- Leader actions shall include `<Space>ca` for code actions, `<Space>cr` for
  rename, and `<Space>cf` for formatting.
- Requests superseded by typing or later cursor movement shall be cancelled or
  ignored.
- Unavailable actions shall explain whether the analyzer is still starting,
  failed, or lacks the capability.

## Requirement 15 — Typed configuration without scripting

The editor shall be fully usable with built-in defaults and support optional
declarative TOML configuration.

- Configuration shall follow XDG paths on Linux.
- Precedence shall be: built-in defaults, user configuration, trusted project
  configuration, then command-line options.
- Configurable areas shall include keymaps, leader-menu labels, theme and UI
  choices, external-tool paths and arguments, and resource limits.
- Key bindings may invoke only registered typed commands, not arbitrary embedded
  code.
- Project configuration capable of changing external commands shall require
  explicit trust.
- The schema shall be versioned and strongly validated, with file locations
  shown for invalid values or unknown keys.
- A failed reload shall retain the last valid configuration.
- Configuration shall support explicit live reload without restarting the
  editor.
- A command shall display the effective configuration and where each overridden
  value came from.
- Configuration shall never load Lua, JavaScript, Python, shared libraries, or
  editor plugins.

## Requirement 16 — Minimal persistent UI

The default TUI shall expose essential state without dashboards, animations, or
decorative plugin-style chrome.

- Each editor pane shall have a gutter with relative line numbers, an absolute
  number on the cursor line, and diagnostic signs.
- A single status line shall show the current mode, file path,
  modified/read-only state, cursor position, `rust-analyzer` status, and
  `codex-watch` status.
- A bottom command/message line shall handle Ex commands, search input, errors,
  and short-lived notifications.
- Messages shall also be retained in a bounded history for later inspection.
- Narrow terminals shall hide lower-priority fields before reducing the usable
  editing area below a practical minimum.
- Status changes shall redraw only affected cells; idle spinners or animations
  shall not cause continuous rendering.
- The built-in theme shall be readable in true color and 256-color terminals,
  with all UI colors configurable.
- There shall be no startup dashboard; launching without a file shall open an
  empty scratch buffer immediately.

## Requirement 17 — Undo, repeat, and macros

Editing shall use explicit transactions that support Vim-like undo behavior.

- One logical change—such as an Insert-mode session, operator command,
  completion, formatting action, or code action—shall normally form one undo
  step.
- `u` and `Ctrl-r` shall traverse undo and redo history.
- Divergent edits after undo shall create a branch rather than destroying the
  previous redo history.
- `g-`, `g+`, `:earlier`, and `:later` shall navigate the undo tree.
- `.` shall repeat the previous semantic edit against the current context rather
  than replaying raw terminal input.
- `q{register}` shall record a macro, `@{register}` shall play it, and `@@` shall
  repeat the latest macro.
- Macro playback shall support counts, be interruptible, and enforce
  recursion/work limits to prevent lockups.
- Undo history shall be stored privately, bounded by configurable limits, and
  persist across normal editor restarts.
- Recovery data and persistent undo data shall remain separate so recovering a
  crash cannot corrupt established history.

## Requirement 18 — Text model and size boundaries

The editor shall represent text and positions without ambiguity across storage,
display, editing commands, and LSP messages.

- Byte offsets, Unicode scalar positions, grapheme positions, terminal display
  columns, and LSP UTF-16 positions shall use distinct internal types with
  thoroughly tested conversions.
- Cursor movement and character-wise edits shall operate on extended grapheme
  clusters and never split a valid UTF-8 sequence.
- Block selections shall operate on display columns without splitting wide
  graphemes or tab cells.
- Every committed transaction shall advance a monotonic buffer version used to
  reject stale background and LSP results.
- A local edit shall not require copying or rescanning the complete buffer.
  Background consumers shall receive consistent snapshots without locking the
  input path.
- Normal functionality, including highlighting and analysis, shall support
  files up to 10 MiB or 500,000 lines.
- Files up to 100 MiB shall remain viewable and editable in a clearly indicated
  large-file mode. Expensive parsing, LSP features, semantic decoration, and
  persistent undo may be disabled or more tightly bounded in that mode.
- Files beyond the configured safety limit shall produce a clear prompt or
  error rather than risking uncontrolled memory use.

## Requirement 19 — Clipboard and baseline Rust editing conveniences

Internal registers and editing shall remain fully functional without access to
a desktop clipboard.

- Vim registers `"+` and `"*` shall integrate with the system clipboard and
  primary selection when the environment supports them.
- Clipboard access shall use a replaceable provider, run off the input path, and
  fail with clear feedback while preserving the internal register contents.
- Character-wise, line-wise, and block-wise register types shall survive copy,
  paste, and macro operations without unintended newline conversion.
- Bracketed terminal paste shall insert literal text as one undo transaction and
  shall never interpret pasted content as Normal-mode commands.
- New lines shall preserve surrounding indentation, with Rust defaults of four
  spaces and no hard tabs unless configuration or detected file settings say
  otherwise.
- `>>`, `<<`, and `=` shall indent, dedent, and reindent through the normal
  operator grammar.
- `gcc` and `gc` with a motion or selection shall toggle language-appropriate
  comments.
- `rust-analyzer` formatting shall apply as one undoable transaction. Optional
  format-on-save shall be available but disabled by default so saving does not
  make unrequested edits.
- Automatic insertion of matching delimiters and quotes is not required for the
  initial release.

## Requirement 20 — CLI, project selection, and sessions

The command-line interface shall support `editor [PATH ...]`, directories,
standard input via `-`, `+LINE[:COLUMN] FILE`, `--help`, and `--version`.

- Explicit file arguments shall open as buffers, and a directory argument shall
  establish the initial project root and open the project explorer.
- Project-root precedence shall be the containing Cargo workspace, then the
  nearest Git root, then the supplied directory or process working directory.
- Standard-input content shall open in a scratch buffer and shall require an
  explicit destination before saving.
- The editor shall remain one foreground process; session support shall not
  require a resident service or singleton instance.
- On clean exit, per-project session state shall atomically record open file
  paths, pane layout, active buffer, cursors, viewports, and explorer state, but
  never duplicate file contents.
- Launching a project without explicit files shall restore its last clean
  session asynchronously after the first frame. Explicit file arguments shall
  take precedence, and `--no-session` shall disable restoration.
- Dirty-buffer restoration shall use the recovery mechanism from Requirement
  12, never ordinary session data.
- Session files shall be private, versioned, bounded, and stored under the
  appropriate XDG state directory.

## Requirement 21 — Default `<Space>` hierarchy

The built-in leader hierarchy shall be compact, mnemonic, and populated only by
commands available in the current context.

| Prefix | Purpose | Required defaults |
| --- | --- | --- |
| `<Space><Space>` | Project files | Open the project-file finder |
| `<Space>/` | Project text | Open streaming project grep |
| `<Space>e` | Explorer | Open, focus, or close the project explorer |
| `<Space>t` | Terminal | Toggle the PTY-backed bottom terminal panel |
| `<Space>b` | Buffers | `bb` switch, `bd` close, `bn` next, `bp` previous |
| `<Space>c` | Code | `ca` action, `cf` format, `cr` rename |
| `<Space>f` | Files | `ff` find project file, `fr` open a recent file |
| `<Space>s` | Search | `sg` grep, `ss` document symbols, `sS` workspace symbols, `sm` messages |
| `<Space>x` | Diagnostics | `xX` buffer diagnostics, `xx` workspace diagnostics |
| `<Space>u` | UI | `uh` toggle inlay hints |
| `<Space>w` | Windows | `wd` close, `wo` keep only, `w-` split below, `w|` split right |
| `<Space>a` | `codex-watch` | `at` start/stop, `as` status, `ar` restart, `ao` run once, `al` logs, `ad` dry-run, `aw` workspace-write |

`<Space>-` and `<Space>|` shall remain direct aliases for splitting below and to
the right. Dry-run and workspace-write controls shall appear within the
`<Space>a` menu only when `codex-watch` is stopped, and changing either shall
require confirmation. User configuration may remap commands or groups while
retaining validation for collisions and unreachable bindings.

## Requirement 22 — Process security, diagnostics, and privacy

External tool integrations shall be treated as supervised, fallible processes rather than
trusted extensions of the editor core.

- Tool processes shall be launched directly with explicit argument vectors,
  never through an implicit shell. An integrated terminal may directly launch
  the user's configured shell only after an explicit terminal command.
- The resolved executable, arguments, project root, and relevant mode flags
  shall be inspectable before enabling a project integration.
- Untrusted project configuration shall not alter commands, arguments,
  environment variables, or workspace-write permissions.
- Child processes shall run in owned process groups and receive bounded shutdown
  time before forced termination when the editor exits.
- Stdout, stderr, LSP frames, and JSON events shall have configurable size and
  rate limits; malformed input shall disable the integration rather than crash
  the editor.
- Logs shall be bounded and shall omit environment contents, clipboard data, and
  source text by default. Protocol tracing that includes source shall require an
  explicit temporary opt-in.
- `:checkhealth` shall report terminal capabilities, effective paths, project
  trust, tool resolution and versions, integration status, state-directory
  access, and recent performance measurements.
- The editor itself shall perform no telemetry, update checks, crash uploads, or
  network access. Any network behavior of a launched tool remains explicit in
  that tool's configuration and status.

## Requirement 23 — MVP boundary and release criteria

The initial release intentionally excludes full Vim/Neovim compatibility,
Vimscript or Lua, a plugin/package manager, a Git client, debugger integration,
remote or collaborative editing, a GUI, general-purpose
task running, and first-class language integrations beyond Rust. These are not
architectural promises against future work, but none may delay the focused Rust
workflow.

The MVP is complete only when:

- A release build can open, edit, navigate, undo, recover, and safely save a
  representative Rust workspace using the documented modal command set.
- Project exploration, file finding, grep, buffers, and splits work without
  optional external utilities.
- The required `rust-analyzer` operations and supervised `codex-watch` workflow
  pass end-to-end tests, including missing, slow, malformed, and crashed process
  cases.
- Unicode editing, coordinate conversion, modal composition, file persistence,
  and recovery have property, regression, or fuzz coverage appropriate to their
  risk.
- Terminal rendering has deterministic snapshot tests and restores terminal
  state after all recoverable exit paths tested by the harness.
- Startup and input benchmarks satisfy Requirement 2 on the target laptop, and
  benchmark regressions are visible in development and CI.
- Every native-code dependency has the assessment required by Requirement 1,
  with accepted tradeoffs recorded in this document.
- No known defect can silently corrupt, overwrite, or discard user text.
