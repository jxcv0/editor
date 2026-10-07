# Command reference

The editor implements a deliberately bounded Vim-like language. Only commands
listed here are part of the current contract; it does not load Vimscript,
Neovim configuration, mappings, or plugins. `<C-x>` means Ctrl+x, `<Space>` is
the space bar, and `[count]` is an optional decimal prefix.

Each entered decimal count is capped at 1,000,000. Operator and motion counts
multiply, so `2dw` and `d2w` both delete two words; the multiplied product is
not currently capped again. `Esc` cancels a pending operator, count, register,
find, `g`/diagnostic prefix, leader sequence, prompt, or selection.

This reference distinguishes editing-core support from external integration.
An LSP binding may be registered and visible while its final UI action is
unavailable; in that case the command must report status instead of editing
text.

## Desktop frontend

Build with `cargo build --release --features gui` and launch
`target/release/editor --gui [PATH ...]`. All startup arguments and the modal
commands below also apply to the desktop frontend. A build without `gui`
reports how to enable it before loading files or entering a terminal.

FiraCode Nerd Font Mono is embedded and selected by default, with programming
ligatures in the workspace. Each character still has its own cursor/click
position. Ligatures stop at changes in styling and at non-ASCII grapheme
boundaries. egui's bundled fallback fonts remain available for other glyphs.
No font installation or configuration is required. The titlebar's **editor**
menu opens the bundled font license in a scratch buffer.

| Desktop input | Action |
| --- | --- |
| Ctrl-P | File finder. |
| Ctrl-S | Save the current buffer through the usual conflict/formatting path. |
| Ctrl-Shift-C | Copy the Visual selection (including block selections), or last yank, to the system clipboard. |
| Ctrl-Shift-V | Paste from the system clipboard. Ctrl-V still enters Visual block mode. |
| Ctrl-plus/minus/0 | Enlarge, shrink, or reset editor font size. |
| Click a tab / explorer entry | Switch buffers / open a file or toggle a directory. |
| Click / drag in source | Move the grapheme cursor / select characters in one pane. Clicking ends an open Insert transaction. |
| Wheel over the workspace | Navigate the focused editor/list/diff, or scroll the focused terminal history. |
| Drop a file | Open it through the normal UTF-8/file-size checks. |
| Window close | Quit if clean; otherwise show keep-editing/discard choices for all dirty buffers. Esc cancels the dialog. |

The toolbar and rail expose save, finder, search, Git, split, terminal, and
Cargo commands. Ctrl-P, Ctrl-S, and font shortcuts are passed through when the
terminal has focus; clipboard shortcuts belong to the desktop. Core selection
is separate from terminal selection (the latter is not implemented). IME
preedit is displayed without changing text; committed text uses core input.
The explorer is resizable, source colors use `ui.theme`, and font zoom is not
persisted. Dialogs, pointer input across window systems, clipboard providers,
and IME should also be exercised on the target desktop before release.

## Normal mode

### Movement

| Keys | Action |
| --- | --- |
| `[count]h`, `j`, `k`, `l` | Move left, down, up, or right by grapheme/line. Arrow keys are aliases. |
| `[count]w` | Move to the next word/punctuation run. |
| `[count]b` | Move to the previous word/punctuation run. |
| `[count]e` | Move to the end of the current/next word or punctuation run. |
| `0` | First grapheme of the line. |
| `^` | First non-whitespace grapheme of the line. |
| `[count]$` | End of this line, or the end of the later line selected by a count. |
| `gg` | First line; `[count]gg` goes to that one-based line number. |
| `G` | Last line; `[count]G` goes to that one-based line number. |
| `%` | Find the first `()`, `[]`, or `{}` delimiter at/after the cursor on this line and move to its byte-balanced mate. |
| `[count]f{char}`, `F{char}` | Find the next/previous occurrence on the current line. |
| `[count]t{char}`, `T{char}` | Move until just before/after the next/previous occurrence on the current line. |
| `[count];`, `[count],` | Repeat the last character find in the same/opposite direction. |
| `[count]<C-d>`, `<C-u>` | Move down/up 12 lines per count. |
| `[count]<C-f>`, `<C-b>` | Move down/up 24 lines per count. Page Down/Up use the same 24-line movement. |

Vertical movement remembers a grapheme column, not a terminal display column.
`%` is a lightweight delimiter matcher: it does not parse Rust syntax and may
count delimiter bytes inside strings or comments.

### Insert entry and direct changes

| Keys | Action |
| --- | --- |
| `i`, `I` | Insert at the cursor, or at the first nonblank grapheme. |
| `a`, `A` | Insert after the cursor, or at end of line. |
| `o`, `O` | Open an indented line below/above and enter Insert mode. |
| `[count]D` (Shift-D) | Change from the cursor through end of line and enter Insert mode, like `c$`; a count extends through that many lines. On an empty line, enter Insert without removing its newline. This intentionally differs from Vim's `D`. |
| `[count]x` | Delete graphemes on the current line into the selected and unnamed registers. |
| `[count]r{char}` | Replace graphemes on the current line. |
| `[count]J` | Join lines, replacing the boundary/leading whitespace with one space. |
| `[count]p`, `[count]P` | Put the selected register (unnamed by default) after/before the cursor. |
| `[count]u`, `[count]<C-r>` | Move backward/forward through the current undo branch. |
| `[count]g-`, `[count]g+` | Current undo/redo aliases; chronological undo-tree traversal is not implemented. |
| `[count].` | Repeat the last supported semantic change. |

The repeat command currently records Insert text, `x`, `r`, `J`, puts, most
non-find delete motions with their counts, and repeated-line deletes. It is not
a byte-for-byte replay and does not yet cover every change type. Deletes made
with `f`/`F`/`t`/`T` or a text object are currently remembered as a word delete,
so `.` is not semantically exact for those cases. It re-inserts recorded Insert
text at the current position but does not preserve the entry semantics of `c`/`D`,
`a`/`A`, or `o`/`O`; indentation, comments, formatting, and arbitrary Visual
edits are also not repeatable yet.

### Search and diagnostics

| Keys | Action |
| --- | --- |
| `/pattern<Enter>` | Search forward using Rust `regex` syntax. |
| `?pattern<Enter>` | Search backward using Rust `regex` syntax. |
| `[count]n`, `[count]N` | Repeat in the same/opposite direction, wrapping through the buffer. |
| `*`, `#` | Search forward/backward for the word under the cursor, escaped and surrounded by word boundaries. |
| `[count]]d`, `[count][d` | Move to a later/earlier current-version diagnostic for the current file, wrapping when needed. |
| `gl` | Show current-version diagnostics on the current line in the message area. |
| `K`, `gd`, `gD`, `gy`, `gI`, `gr` | Request hover, definition, declaration, type definition, implementation, or references from `rust-analyzer`. |

Invalid regular expressions and missing search patterns produce a message.
An entirely empty diagnostics set also reports; if workspace diagnostics exist
but none match the active path/current version, `[d` and `]d` currently do
nothing silently. Search is line-oriented and uses Rust-regex syntax, not Vim's
regular-expression dialect. Search highlighting and an incremental search
preview are not yet implemented.

In Normal mode, current-version diagnostics are also rendered beside the affected source line,
ordered by severity and clipped to the pane width. `gl` shows the collected
diagnostic messages for the cursor line. `K`, `gd`, `gD`, `gy`, `gI`, and `gr`
issue distinct typed requests for hover, definition, declaration, type
definition, implementation, and references. They require a ready
`rust-analyzer`. `K` (Shift-K) opens a bordered hover box above the cursor,
or below when there is insufficient room above. It preserves paragraphs and
code lines, wraps within the pane, and supports `<C-f>`/`<C-b>` scrolling.
`Esc` dismisses it; any editing/navigation key dismisses it and performs its
usual action. Late hover results are discarded after further input.
Inlay hints are requested automatically for the current Rust revision and
rendered inline in Normal mode; neither hints nor diagnostic text interrupt
Insert mode. Diagnostic gutter markers remain visible.
`gr` opens a references
picker, including for a single result, with each file/line/column followed by a
source-code excerpt. Open buffers supply their current unsaved text; other files
load previews asynchronously. Type to filter by path or code, then use `Enter`
or a split-open key to jump to the exact symbol column. Other navigation requests
open one location directly or show a picker for multiple locations. Request
cancellation remains minimal.

Native rust-analyzer diagnostics follow unsaved text revisions. Diagnostics
from disk-backed `rustc` or Clippy checks are hidden after an unsaved edit and
are refreshed through `textDocument/didSave` after `:w` succeeds.

## Insert mode

`Esc` or `<C-[>` returns to Normal mode and commits the entire Insert session
as one undo step. Ordinary typed characters insert literally. The following
keys are also handled:

| Keys | Action |
| --- | --- |
| `Enter` | Insert a newline and copy the current line's leading whitespace. |
| `Tab` | Insert `editor.tab_width` spaces, or one tab when `insert_spaces = false`. |
| `Backspace`, `Delete` | Delete one grapheme backward/forward. |
| Arrow keys, `Home`, `End` | Move without leaving Insert mode. |
| `<C-Space>` | Request completion from ready `rust-analyzer` and open a result picker. Most terminals send `` Ctrl-` `` as this same key, so it does not toggle the terminal in Insert mode. |
| `<C-n>`, `<C-p>` | Select the next/previous item while the completion picker is open. |
| `Enter`, `Esc` | Accept the selected completion, or dismiss the picker and return to Insert mode. |

Completion is manual only. Typing or deleting while its picker is visible
dismisses it and applies that key normally. Candidate details are stored but
not fully rendered, LSP snippets are inserted as literal `insertText`,
`<C-e>` does not yet dismiss an open picker, and placeholder/signature-help
navigation is not implemented.

Bracketed terminal paste is inserted literally as part of one Insert
transaction. A paste received outside Insert mode begins an Insert session; it
is never interpreted as Normal-mode commands.

## Operators and text objects

The operators `d` (delete), `c` (change), and `y` (yank) compose with the
motions `h j k l w b e 0 ^ $ gg G % f F t T`. Repeating an operator makes a
line operation: `dd`, `cc`, and `yy`. Examples include `dw`, `2dw`, `d2w`,
`c$`, `ygg`, and `df)`.

Indentation operators are also available:

| Keys | Action |
| --- | --- |
| `>>`, `<<` | Indent/dedent lines by the configured width. |
| `==` | Reindent lines using a lightweight brace-depth heuristic. |
| `gc{motion}`, `gcc` | Toggle line comments (`#` for TOML and `//` otherwise). |

`d`, `c`, and `y` support these `i` (inner) and `a` (around) objects:

| Objects | Meaning |
| --- | --- |
| `w`, `W` | Word/punctuation run, or whitespace-delimited WORD. |
| `"`, `'`, `` ` `` | Quoted text on the current line. |
| `(`, `)`, `b` | Parenthesized pair. |
| `[`, `]` | Bracketed pair. |
| `{`, `}`, `B` | Braced pair. |

Examples: `ciw`, `daW`, `yi"`, `ca(`, `di[`, and `yaB`. Pair matching is
byte-balanced rather than syntax-aware, and quote objects do not interpret
escape sequences. Text objects are currently accepted only after an operator,
not as standalone Visual-mode object expansion. A composed count is currently
ignored for a text object, so `2diw` behaves like `diw`; counts do compose with
the supported motions and repeated-line forms.

## Visual modes

| Keys | Action |
| --- | --- |
| `v` | Enter/toggle Visual character mode. |
| `V` | Enter/toggle Visual line mode. |
| `<C-v>` | Enter Visual block mode. |
| `h j k l w b e 0 ^ $ gg G % f F t T` | Extend the active selection. Arrow/page aliases are not handled in Visual mode. |
| `d` or `x`, `c`, `y` | Delete, change, or yank the selection. |
| `>`, `<`, `=` | Apply a line transform to every selected line in any Visual mode. |
| `:` | Open the ordinary Ex prompt; no Visual range is inserted. |
| `<Space>` | Open the leader menu. |
| `Esc` | Leave Visual mode without changing text. |

Visual block support is an early subset. Blocks are currently based on
grapheme indices rather than terminal display columns; short lines are clipped,
wide/tab cells are not padded, and block change enters one Insert cursor rather
than multi-line insertion. Block `>`, `<`, and `=` intentionally operate on
whole selected lines rather than rectangular cells. A later Normal-mode block
put also uses grapheme indices, clips short lines, and does not create missing
lines. `gc` is not wired for an existing Visual selection. Character/line/block
yank exits to Normal mode.

## Registers and macros

`"{register}` selects a register for the next operator or put. The unnamed
register `"` and named registers `a`-`z` are supported. Uppercase names are
accepted but normalized to lowercase; unlike Vim, uppercase does not append.
The `+` and `*` names exist as internal registers only—there is no system
clipboard/primary-selection provider in the current build. Numbered, small
delete, black-hole, expression, and read-only Vim registers are unsupported.
An explicitly selected empty register reports that it is empty; it does not
fall back to the unnamed register, and the prefix is consumed by that failed
put.

| Keys | Action |
| --- | --- |
| `q{a-z}` ... `q` | Begin recording keys into a macro slot; return to Normal mode and press `q` to stop. |
| `[count]@{a-z}` | Play a recorded macro. |
| `[count]@@` | Repeat the last played macro. |

Macro slots are separate from text registers and are not persisted. Playback
is limited to recursion depth 16 and 10,000 key applications per invocation.

## Leader menu

In Normal or Visual mode, `<Space>` enters the leader menu. `Esc` closes it;
an unknown sequence makes no edit and reports the sequence. The built-in
registry is:

| Sequence | Command | Current behavior |
| --- | --- | --- |
| `<Space><Space>`, `<Space>ff` | Find project file | Opens the fuzzy file picker over files already streamed by the project scan. |
| `<Space>/`, `<Space>sg` | Project grep | Opens streaming regex project search; changing the query cancels the previous task. |
| `<Space>e` | Explorer | Open/focus it, or close it when it is already focused. |
| `<Space>bb` | Switch buffer | Opens the fuzzy buffer picker. |
| `<Space>bd` | Close buffer | Refuses a modified buffer. |
| `<Space>bn`, `<Space>bp` | Next/previous buffer | Cycles the buffer list. |
| `<Space>ca` | Code action | Requests actions and lists their titles; selecting/executing an action is not complete. |
| `<Space>cf` | Format | Applies returned active-buffer text edits as one transaction. |
| `<Space>cr` | Rename | Opens `:rename ` input. Returned edits for the active buffer are applied; multi-file edits are not. |
| `<Space>cR` | Restart rust-analyzer | Restarts the client; remains available while the server is failed or not ready. |
| `<Space>cc` | Cargo check | Runs `cargo check` now and shows the right-side check panel. |
| `<Space>cw` | Watch cargo check | Toggles the check panel. While it is visible, saving a Rust source or Cargo manifest/lockfile re-runs the check; hiding it stops watching and cancels a running check. |
| `<Space>Cr`, `<Space>Ct`, `<Space>Cb`, `<Space>Cc` | Cargo run/test/build/check | Run directly and show output in the right-side panel. Check also enables save watching. |
| `<Space>Cu`, `<Space>Cl`, `<Space>Cf` | Cargo update/Clippy/fmt | Run directly; fmt changes reload into clean buffers when finished. |
| `<Space>Cd`, `<Space>Cx`, `<Space>Cq` | Cargo doc/clean/cancel | Build docs, clean build outputs, or cancel the current Cargo command. |
| `<Space>fr` | Recent files | Opens files visited during this process; this list is not persistent yet. |
| `<Space>ss`, `<Space>sS` | Document/workspace symbols | Requests and lists symbols. Workspace locations can open; document-symbol selection remains incomplete. |
| `<Space>sm` | Messages | Opens bounded message history. |
| `<Space>xX` | Buffer diagnostics | Opens current-version diagnostics for the active buffer. |
| `<Space>xx` | Workspace diagnostics | Opens all retained workspace diagnostics. |
| `<Space>uh` | Toggle inlay hints | Enable/disable versioned, inline Rust hints in Normal mode. |
| `<Space>ua` | Toggle animations | Toggle smooth scrolling and cursor animation together. Independent `ui.smooth_scroll` and `ui.cursor_animation` settings default to true. |
| `<Space>gs` | Git status | Browse staged, unstaged, untracked, deleted, and conflicted paths. |
| `<Space>gd`, `<Space>gD`, `<Space>gh` | Git diffs | Show the active saved file's unstaged, staged, or HEAD diff. |
| `<Space>gb`, `<Space>gc` | Line history | Show the last commit affecting the current saved line, or its full commit patch. |
| `<Space>ga`, `<Space>gu` | Stage/unstage file | Change the index for the active saved file. Refuse unsaved buffer edits. |
| `<Space>gn`, `<Space>gp` | Next/previous Git hunk | Jump between staged/unstaged changed hunks, wrapping at the ends. Requires current clean-buffer line data. |
| `<Space>-`, `<Space>w-` | Split below | Create a horizontal split showing the same buffer. |
| `<Space>\|`, `<Space>w\|` | Split right | Create a vertical split showing the same buffer. |
| `<Space>wd` | Close pane | Refuses to close the last pane. The buffer remains open. |
| `<Space>wo` | Keep only pane | Remove the other panes without deleting their buffers. |

The menu registry is declarative, but context filtering/disabled annotations
are incomplete. A registered integration command can therefore be shown even
when the tool is missing or starting; invoking it should explain the current
status.

### Git

Git runs directly, asynchronously, with literal path arguments and no pager,
external diff driver, or textconv. Read-only status refreshes after startup,
on active-file changes, after a save, and about every two seconds while idle.
The foreground accepts line data only for the matching clean buffer revision
and unchanged disk metadata. It hides those markers immediately on editing.
Repository status uses the two-column [Git porcelain format](https://git-scm.com/docs/git-status):
the first column is the index (staged), the second is the working tree
(unstaged), and `??` means untracked. Renames appear as deletion/addition
pairs. Branch/upstream information appears in the header; status-bar `S` and
`U` count files in each category, including untracked files in `U`.

The active-file gutter has separate staged and unstaged columns after the
diagnostic column. `+` is addition, `~` replacement, and `-` deletion, anchored
to the preceding surviving line (or the first line at the top). Staged marks
are mapped from index coordinates through unstaged edits; replaced/deleted
index lines have no staged mark on the new working text. The staged diff
remains available for reviewing them. Binary changes and merge conflicts
appear in status/diff views but do not provide ordinary text-hunk markers.

| Keys in the Git view | Action |
| --- | --- |
| `j`/`k`, Down/Up | Select a status entry or scroll a diff. |
| Page Down/Up, Ctrl-f/Ctrl-b | Move 12 entries/rows. |
| `g`, `G` | First/last entry or top/bottom of a diff. |
| `h`/`l`, Left/Right | Scroll horizontally by eight display cells in split diffs, or graphemes in unified/history views. |
| `d`, Enter | Unstaged diff for the selected file. |
| `D`, `H` | Staged diff, or diff between HEAD and the saved file. |
| `s`, `u` | Stage/unstage the selected file, then refresh status. |
| `o` | Close the view and open the selected file. |
| Backspace | Return from a diff or history view to status. |
| `r` | Refresh repository status. |
| `q`, Esc | Close the Git view. |

The view consumes input and paste without editing buffers. While loading,
Esc/`q` can dismiss it; late results never reopen it. Ctrl-backtick dismisses
the view and toggles the terminal. Diffs are read-only snapshots of saved
files; save first to include buffer edits. Stage/unstage operate on entire
files and reject dirty open buffers. Unstaging preserves working-tree content,
including in repositories without commits. An unborn repository supports
status, staged/unstaged diffs, and staging, but has no HEAD/line history.
Untracked text is shown as an addition. Deleted paths can be staged from
status. Current-line history reports uncommitted lines instead of attributing
them to an unrelated commit.

At 64 or more cells wide, patches use aligned BEFORE/AFTER columns, each with
its own source line numbers. Contiguous removed/added blocks are paired in
order; unequal blocks have blank cells so following context remains aligned.
This is a bounded linear alignment, not similarity matching between unrelated
lines. Both columns scroll together, tabs expand at four-cell stops, and wide
or combining graphemes are clipped without shifting the other column. Missing
final newlines are marked on the affected side. Narrow views use the original
unified text. Commit patches can contain multiple files; file headers, binary
diff notices, renames, empty diffs, and truncation notices remain visible.

There is one Git worker plus at most one queued explicit request. Each child
has a ten-second timeout and an output limit of `limits.tool_message_bytes`
(clamped to 1 KiB–32 MiB); exceeding it reports an error. Status is limited to
10,000 entries, and diff/history presentation to 50,000 lines with an explicit
truncation notice. Git workers are cancelled and joined on shutdown. Missing
Git or an unavailable repository produces a message for explicit commands;
background failures clear stale Git state without interrupting editing.

### Integrated terminal

The first `:terminal` or `` Ctrl-` `` starts `$SHELL` in the project root through
a PTY. Hiding and reopening the panel preserves that shell and its screen.
Terminal-focused keys and paste are sent to the PTY. `` Ctrl-` `` toggles the
panel from editor, explorer, picker, prompt, and terminal focus, leaving any
pending Visual, Leader, operator, prompt, or picker state first. `Ctrl-\`
returns focus to the editor without hiding the panel; `Ctrl-w j` focuses it
again. From editor focus, `:terminal` also hides the panel. `Shift-PageUp` and
`Shift-PageDown` move through bounded scrollback by a page, while `Shift-Home`
and `Shift-End` jump to its top and bottom. Mouse reporting, terminal text
selection, and clipboard integration are not yet implemented.

`Ctrl-D` is forwarded as EOF to the shell or foreground program. At an empty
shell prompt it normally exits the shell; shell exit (including `exit`) closes
the panel and returns terminal focus to the editor. Toggling the terminal again
starts a fresh shell. Inside a running program, `Ctrl-D` keeps that program's
normal EOF behavior, and the panel stays open while the shell is running.

Without an enhanced keyboard protocol, terminals encode `` Ctrl-` `` as NUL, the
same byte as `Ctrl-Space` and `Ctrl-@`, so those keys toggle the panel too and
are not forwarded to the shell. In Insert mode that byte keeps its
`<C-Space>` completion meaning; a distinctly reported `` Ctrl-` `` still leaves
Insert mode and toggles the terminal.

### Cargo panel

`<Space>C` opens the Cargo submenu. `:cargo COMMAND [args]` runs the same
commands; arguments are separated by whitespace and passed literally, without
shell expansion or quote parsing. For example, `:cargo test --lib` or
`:cargo run --example demo`. Each run uses `tools.cargo.path` and appends
`tools.cargo.args`; those configured flags must be valid for the chosen command.
Program/test output appears during execution, preserving repeated lines, with
at most 1,000 entries retained. Stdin is closed, so interactive programs belong
in the integrated terminal. Starting another command, hiding the panel, or
exiting the editor cancels the current process group. Only check commands
enable save watching. Other commands cannot be restarted by a save.
Clean buffers changed by completed commands reload immediately; dirty buffers
remain protected. `<Space>Cq`, `:cargo cancel`, or `s` inside the panel cancels
the current run.

`<Space>cc` runs `cargo check --message-format=json` (plus `tools.cargo.args`)
in the project root and opens the panel docked right of the panes. It needs at
least 59 columns beside the explorer and otherwise stays hidden. The header
shows the latest status line while a run is in progress, then the error and
warning counts. Each entry lists its rustc header, location, primary label, and
notes, errors before warnings, followed by cargo's own output such as manifest
errors. Duplicate diagnostics are shown once and at most 1,000 entries are
kept. A new run replaces the displayed results only when it finishes.

`<C-w>l` or `<C-l>` from the last (rightmost) pane focuses the panel. When it
has focus:

- `j`/`k`, Down/Up, or `<C-n>`/`<C-p>` select an entry; `<C-d>`/`<C-u>` and
  PageDown/PageUp move by ten; `g`/`G` or Home/End select the first or last.
- `Enter`, `o`, or `l` open the selected entry's file at rustc's line and
  column and return focus to the editor.
- `r` re-runs the current Cargo command with the same arguments; `s` stops it.
- `<Space>` opens the leader menu and `:` the command line from editor focus.
- `Esc`, `q`, `<C-h>`, or `<C-w>h` return focus to the editor; `<C-w>j` focuses
  a visible terminal.

### Pickers

Type to refine a picker, use Up/Down or `<C-p>`/`<C-n>` to move, `Enter` to
open, `<C-v>` to open in a vertical split, `<C-s>` to open in a horizontal
split, and `Esc` to close. File results are fuzzy-ranked; buffer, recent,
message, diagnostic, symbol, and reference lists use fuzzy filtering in their
existing order. The file finder starts from the startup index; opening it
rescans the project in the background, at most once every two seconds, keeping
the current results searchable, adding new files as they are found, and
removing files that no longer exist once the rescan completes. Saved files and
re-listed explorer directories are added to the index immediately. Project grep
is a separate cancellable asynchronous regex operation
rather than a shell call to `ripgrep`. Reference results show source-code
excerpts beneath their locations. Disk previews are limited to 8 MiB per file
and excerpts to the first 16 KiB of a line; unavailable previews retain the
selectable location. Other picker details/previews are not currently drawn.

### Explorer

The explorer displays an indented tree with directories sorted before files.
`▸` marks a collapsed directory and `▾` marks an expanded one. Directory contents
load in the background when expanded, including empty directories.

When the explorer has focus:

- `j`/`k` or Down/Up select a visible entry.
- `Enter` toggles a directory or opens a file.
- `l` or Right expands a directory, enters its first child if already expanded,
  or opens a file.
- `h` or Left collapses an expanded directory or selects the parent. At the
  top level it leaves the selection in place.
- `Esc`, `<C-l>`, and `<C-w>l` return focus to the editor.
- `e` toggles hidden paths and `i` toggles ignored paths, refreshing the tree.
- `<Space>e` closes the explorer while it has focus.

Opening or focusing the explorer reveals the active file by expanding its
ancestors. Expanded directories are remembered in project sessions. Creation,
renaming, and deletion are not performed from the explorer; deletion reports
that explicit filesystem confirmation is unavailable. Saving a file re-lists
its directory immediately, so a new file appears at once. Every two seconds
while idle, each listed directory's modification time is compared with the
one recorded when it was listed; changed directories are listed again in the
background, which shows files created, renamed, or removed elsewhere (for
example in the integrated terminal).

## Ex commands

When `[editor] format_on_save = true`, Rust writes through `:w`, `:wq`,
`:x`, and `:saveas` request formatting before the final save. Edits update the
visible buffer as one undo step. The write retains normal disk-conflict checks,
and `:wq` closes its original pane only after a successful write. Further edits
or changing panes while formatting is pending cancel that save. Missing or
failed servers and three-second timeouts fall back to saving unformatted text
and report the reason. An unnamed buffer is first saved to establish its URI.

Paths are resolved from the active project root. Long names and listed short
aliases are accepted.

| Command | Action |
| --- | --- |
| `:w[!] [PATH]`, `:write[!] [PATH]` | Save, or save as `PATH`. Without `!`, refuse an external-change conflict. |
| `:q`, `:quit` | Close the current pane. Refuse if it is the only view of a modified buffer. On the last pane, quit only when every buffer is clean. |
| `:q!`, `:quit!` | Close the current pane, discarding its modified buffer only if this is its last view. On the last pane, switch to another unsaved buffer if one remains; otherwise quit. |
| `:qa`, `:qall` | Quit the editor only if every buffer is clean. |
| `:qa!`, `:qall!` | Quit the editor and explicitly discard all unsaved buffers. |
| `:wq[!] [PATH]`, `:x[!] [PATH]` | Save the active buffer, then close the current pane. A failed save keeps the pane open. Other unsaved buffers prevent exiting the last pane. |
| `:e[!] [PATH]`, `:edit[!] [PATH]` | Open `PATH`; with no path, reload the current file. `!` permits discarding local changes for reload. |
| `:saveas[!] PATH` | Save the buffer under another name. |
| `:b [NUMBER\|NAME]`, `:buffer ...` | Open the buffer picker, or switch by one-based number/name substring. |
| `:bn`, `:bnext`; `:bp`, `:bprevious` | Cycle buffers. |
| `:bd[!]`, `:bdelete[!]` | Close the current buffer; `!` explicitly discards changes. |
| `:split [PATH]`, `:sp [PATH]` | Split horizontally and optionally open a file. |
| `:vsplit [PATH]`, `:vs [PATH]` | Split vertically and optionally open a file. |
| `:only` | Keep the active pane. Open buffers are retained. |
| `:earlier [N]`, `:later [N]` | Undo/redo `N` nodes on the current branch. |
| `:messages` | Open message history. |
| `:terminal`, `:term` | Toggle the integrated terminal panel. |
| `:git`, `:git status` | Open Git status. |
| `:git diff`, `:git staged`, `:git head` | View the active saved file against the index, HEAD against the index, or HEAD against the saved file. |
| `:git blame`, `:git commit` | Show current-line commit metadata or that commit's patch. |
| `:git stage`, `:git unstage` | Stage/unstage the active saved file. |
| `:git next`, `:git prev`, `:git refresh` | Move between changed hunks, or refresh status/active-file markers without opening the view. |
| `:check`, `:cargocheck` | Run `cargo check` and show the check panel. |
| `:cargo COMMAND [args]` | Run check, run, test, build, update, clippy, fmt, doc, clean, or cancel. |
| `:rename NEW_NAME` | Request an LSP rename and apply returned changes for the active buffer only. |
| `:rarestart`, `:lsprestart` | Restart the `rust-analyzer` client. |
| `:checkhealth` | Request the current health report; the report is incomplete in this MVP. |
| `:config` | Report the schema version and loaded config file paths, not the complete effective config. |
| `:reloadconfig` | Reload config; on failure, retain the last valid config. Project tool overrides remain untrusted. |

`:e PATH` opens an existing file or a new, path-associated empty buffer when
the destination does not exist; the latter is created on save. `:w PATH` and
`:saveas PATH` can instead assign a destination to the current scratch buffer.
`:x` currently shares the save-then-quit flow with `:wq`. Plain `:x` does not
rewrite a clean buffer at its existing path, while forced or save-as variants
can publish bytes.
`:q` closes a pane; `:bd` deletes a buffer. Closing one of several panes showing
the same modified buffer preserves its text and undo history, including with
`:q!`. There is no `'hidden'` option: normal `:q` refuses to hide the only view
of a modified buffer. `:q!` does not discard other unsaved buffers; `:qa!` is the
explicit command to discard all and exit.
`:earlier`/`:later` interpret their argument as an undo-step count (a suffix
such as `5m` is not Vim time travel). There is no Ex range grammar, command
chaining, shell escape, substitutions, or autocommand language.
`:reloadconfig` replaces the typed config but does not rebuild existing
undo/explorer state or already running tool workers; some UI/limit settings and
changed tool paths/arguments therefore require a restart.

## Pane navigation deviations

In Normal and Visual modes, `<C-h>`/`<C-k>` select the previous layout leaf and
`<C-j>`/`<C-l>` select the next one. The same directions work after a `<C-w>`
prefix. When the explorer is open, moving left from an editor pane focuses it;
moving right from the explorer focuses the editor. When the cargo check panel
is visible, moving right from the last pane focuses it instead of wrapping. This is deterministic but
not yet geometric left/down/up/right pane selection. Closing a pane retains its
buffer, except when `:q!` explicitly discards a modified buffer's last view.
`<Space>wd` keeps buffers open and still refuses to close the last pane.

## Unsupported-command feedback

Unknown character-based Normal/Visual commands, operator motions, `g` and
bracket-prefixed commands, leader sequences, registers, and Ex commands produce
a bottom-line message and do not substitute another edit. Missing search
targets, empty registers/macros, dirty-buffer close attempts, and unavailable
`rust-analyzer` actions also report a reason.

There are known feedback gaps: unhandled non-character Normal/Visual keys and
unhandled Insert, picker, or explorer keys can be ignored silently, and some
registered integration actions have placeholder messages rather than a
complete unavailable-state explanation. Those are MVP defects relative to
`DESIGN.md`, not intentional Vim compatibility.

## Explicitly out of scope

The current command language does not promise complete Vim/Neovim behavior.
Among other omissions are marks/jumps, substitutions and Ex ranges, tabs,
quickfix/location lists, folds, digraphs, abbreviations, mappings applied from
configuration, autocommands, command-line history/editing, plugin commands,
shell commands, and arbitrary Vim options. Unsupported sequences should be
added deliberately with deterministic tests rather than guessed or aliased to
an unrelated edit.
