# TODOs

- [x] Fix CI: isolate terminal-test project roots, declare Rust 1.88 as the
  minimum, and build/test on both 1.88 and stable, including release builds.
  Hosted GitHub Actions verification is still pending; the API was unreachable
  from the implementation environment.
- [x] Format on save: apply LSP edits to the live buffer before saving, retain
  disk-conflict protection, handle `:wq`/`:saveas`, and reload clean files
  immediately after Cargo tools change them. Enable `editor.format_on_save`.
- [x] Remove terminal from the leader menu. Keep Ctrl-backtick and `:terminal`.
- [x] Show inline LSP diagnostic text only in Normal mode; keep gutter markers.
- [x] Add `<Space>C` and `:cargo` for run, check, test, build, update, clippy,
  fmt, doc, clean, and cancellation with bounded output in the Cargo panel.
- [x] Request and render versioned inlay hints in Normal mode, including
  UTF-16 positions, server refresh requests, and the `<Space>uh` toggle.
- [x] Show Shift-K hover information in a wrapped box above the cursor, with
  a below-cursor fallback, Ctrl-f/Ctrl-b scrolling, and Esc dismissal.
- [x] Close the terminal pane when Ctrl-D exits the shell; keep normal EOF
  behavior inside foreground programs and start a fresh shell when reopened.
- [x] Shift-D in Normal mode changes from the cursor through end of line and
  enters Insert mode, using the same registers and undo transaction as `c$`.
- [x] Add the MIT license and align Cargo package metadata.
- [x] Git integration: branch/status and staged/unstaged file counts, a status
  browser, colored unstaged/staged/HEAD diffs, separate staged/unstaged line
  markers, changed-hunk navigation, current-line blame and commit patches,
  and explicit whole-file stage/unstage. Bounded background Git processes,
  literal paths, dirty/stale line guards, and real repository/PTY tests.
- [x] Smooth scrolling and cursor animations in the existing custom cell
  renderer: time-based easing, bounded travel for large jumps, animation-only
  redraws, immediate logical editing positions, resize/edit snapping, and
  independent configuration plus `<Space>ua`. Terminal motion uses whole
  cells; no TUI dependency or separate graphical renderer is needed.
- [x] Render aligned side-by-side Git diffs with independent old/new line
  numbers, colored replacement blocks, blanks for unequal changes, synchronized
  scrolling, Unicode/tab clipping, missing-newline markers, and a narrow-view
  unified fallback. Includes parser, renderer, and real Git/PTY regressions.
- [x] Add an optional egui/wgpu desktop frontend (`--features gui`, `--gui`):
  charcoal/mint native chrome, vector icons, resizable explorer, buffer tabs,
  toolbar/Cargo actions, GPU text, modal keys, clipboard/IME routing, pointer
  navigation/selection, zoom, and unsaved-close protection over the shared
  runtime. Offscreen Vulkan frames and headless lifecycle/input tests pass;
  interactive Wayland/X11 validation remains environment-dependent.
- [x] Embed FiraCode Nerd Font Mono as the GUI default, with its full license
  available from the editor menu. Render its contextual programming ligatures
  while preserving character cells, syntax/selection boundaries, Unicode
  fallback, and zoom/DPI behavior. Font-program audits, HarfBuzz fixtures,
  cursor/mouse regressions, and offscreen Vulkan ligature screenshots pass.
