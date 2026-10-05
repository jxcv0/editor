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
text at the current position but does not preserve the entry semantics of `c`,
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

Current-version diagnostics are also rendered beside the affected source line,
ordered by severity and clipped to the pane width. `gl` shows the collected
diagnostic messages for the cursor line. `K`, `gd`, `gD`, `gy`, `gI`, and `gr`
issue distinct typed requests for hover, definition, declaration, type
definition, implementation, and references. They require a ready
`rust-analyzer`. Hover is summarized on the message line. `gr` opens a references
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
| `<C-Space>` | Request completion from ready `rust-analyzer` and open a result picker. |
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
| `<Space>t` | Integrated terminal | Open and focus the bottom terminal, or hide it while preserving its shell session. |
| `<Space>bb` | Switch buffer | Opens the fuzzy buffer picker. |
| `<Space>bd` | Close buffer | Refuses a modified buffer. |
| `<Space>bn`, `<Space>bp` | Next/previous buffer | Cycles the buffer list. |
| `<Space>ca` | Code action | Requests actions and lists their titles; selecting/executing an action is not complete. |
| `<Space>cf` | Format | Applies returned active-buffer text edits as one transaction. |
| `<Space>cr` | Rename | Opens `:rename ` input. Returned edits for the active buffer are applied; multi-file edits are not. |
| `<Space>cR` | Restart rust-analyzer | Restarts the client; remains available while the server is failed or not ready. |
| `<Space>fr` | Recent files | Opens files visited during this process; this list is not persistent yet. |
| `<Space>ss`, `<Space>sS` | Document/workspace symbols | Requests and lists symbols. Workspace locations can open; document-symbol selection remains incomplete. |
| `<Space>sm` | Messages | Opens bounded message history. |
| `<Space>xX` | Buffer diagnostics | Opens current-version diagnostics for the active buffer. |
| `<Space>xx` | Workspace diagnostics | Opens all retained workspace diagnostics. |
| `<Space>uh` | Toggle inlay hints | Toggles editor state; requesting/rendering hints is incomplete. |
| `<Space>-`, `<Space>w-` | Split below | Create a horizontal split showing the same buffer. |
| `<Space>\|`, `<Space>w\|` | Split right | Create a vertical split showing the same buffer. |
| `<Space>wd` | Close pane | Refuses to close the last pane. The buffer remains open. |
| `<Space>wo` | Keep only pane | Remove the other panes without deleting their buffers. |

The menu registry is declarative, but context filtering/disabled annotations
are incomplete. A registered integration command can therefore be shown even
when the tool is missing or starting; invoking it should explain the current
status.

### Integrated terminal

The first `<Space>t` starts `$SHELL` in the project root through a PTY. Hiding
and reopening the panel preserves that shell and its screen. Terminal-focused
keys and paste are sent to the PTY. `Ctrl-\` returns focus to the editor without
hiding the panel; `Ctrl-w j` focuses it again. From editor focus, `<Space>t`
hides the panel. `Shift-PageUp` and `Shift-PageDown` move through bounded
scrollback by a page, while `Shift-Home` and `Shift-End` jump to its top and
bottom. Mouse reporting, terminal text selection, and clipboard integration
are not yet implemented.

### Pickers

Type to refine a picker, use Up/Down or `<C-p>`/`<C-n>` to move, `Enter` to
open, `<C-v>` to open in a vertical split, `<C-s>` to open in a horizontal
split, and `Esc` to close. File results are fuzzy-ranked; buffer, recent,
message, diagnostic, symbol, and reference lists use fuzzy filtering in their
existing order. Project grep is a separate cancellable asynchronous regex operation
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
that explicit filesystem confirmation is unavailable. External filesystem
changes do not automatically refresh the tree.

## Ex commands

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
moving right from the explorer focuses the editor. This is deterministic but
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
