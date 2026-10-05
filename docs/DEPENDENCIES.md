# Dependency assessment

This document records the Requirement 1 review for every direct dependency in
`Cargo.toml`. It describes the dependency set represented by `Cargo.lock` on
2026-09-04. The manifest uses compatible-version requirements, while the lock
file fixes the versions used for reproducible application builds.

All first-party implementation is Rust. None of the direct dependencies embeds
a scripting runtime, implements a plugin loader, or contains bundled C/C++
source. Some crates ultimately call the operating-system C/Win32 ABI through
Rust crates such as `libc`, `rustix`, or `windows-sys`; that is a platform
boundary, not an editor extension boundary. No crate in this review has a Cargo
`links` declaration for a third-party native library.

The conclusions below are design-time assessments, not a claim that the
dependency graph is vulnerability-free. `Cargo.lock` should remain committed,
updates should be reviewed, and an advisory scan such as `cargo audit` should
be part of release/CI maintenance.

## Runtime dependencies

### `crossterm` 0.29 (locked: 0.29.0)

- **Purpose:** Raw-mode terminal input, resize and bracketed-paste events,
  alternate-screen lifecycle, cursor control, and styled cell output.
- **Alternatives considered:** Direct ANSI/termios handling would reduce the
  graph but would duplicate subtle cleanup, event, and portability work.
  `termion` is smaller but primarily Unix-oriented. A full TUI framework would
  add layout/widget code the editor does not need.
- **Startup/performance:** Terminal setup is a small fixed startup cost. The
  render path uses queued writes and performs its own cell diff, so Crossterm is
  not asked to redraw a widget tree. Input polling is synchronous but bounded
  by a short timeout. This remains part of the 50 ms startup and 8 ms input
  budgets and needs benchmark evidence.
- **Portability/build:** Rust source with OS-specific Rust dependencies. On
  Unix it reaches terminal/signal APIs through the system ABI; Windows support
  uses `windows-sys`/`crossterm_winapi`. No compiler for bundled C is required.
  Crossterm gives the best path to eventual non-Linux support, though Linux is
  the current target.
- **Security/maintenance:** Terminal escape output and hostile input events are
  the relevant boundary. The editor emits structured Crossterm commands and
  does not interpret terminal input as a shell command. Crossterm is widely
  used, but terminal restore and signal behavior still need dedicated
  integration/harness coverage beyond the present unit tests.
- **License:** MIT.

**Decision:** Accepted. It owns only the terminal adapter and does not leak
into the text model or process integrations.

### `crossbeam-channel` 0.5 (locked: 0.5.16)

- **Purpose:** Bounded, cancellable result streams for project scanning and
  search. Bounded queues prevent background producers from growing memory
  without limit.
- **Alternatives considered:** `std::sync::mpsc` is already sufficient for the
  long-lived LSP controller, but it lacks the same convenient bounded sender and
  timed-send behavior used by project tasks. An async runtime would be much
  larger and would add scheduling machinery to the input path.
- **Startup/performance:** No global runtime or startup work. Channel operations
  add a small synchronization cost on worker/UI handoff and allow the UI to
  drain a bounded number of events without blocking.
- **Portability/build:** Pure Rust, with no native build step; supports the
  editor's desktop targets.
- **Security/maintenance:** Small, mature API and well-established project.
  Queue capacity and cancellation under backpressure are tested locally because
  denial of service through unbounded work is the main risk. Disconnection
  handling does not yet have a focused regression test.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted. Replacing it with `std` is possible later if the project
task abstraction is simplified.

### `dirs` 6 (locked: 6.0.0)

- **Purpose:** Resolve platform-appropriate configuration and state locations,
  including XDG locations on Linux.
- **Alternatives considered:** Reading XDG environment variables and home
  directories directly would be easy on Linux but would repeat fallback logic
  and make future portability less reliable.
- **Startup/performance:** A few environment/path lookups; no scanning, I/O, or
  background service. Impact is negligible compared with reading a config file.
- **Portability/build:** Rust source. Its `dirs-sys` layer selects OS-specific
  Rust crates and may call libc/Win32 APIs; it does not compile bundled native
  code.
- **Security/maintenance:** Paths are influenced by the process environment and
  must not be treated as trusted content. State writes apply private mode bits
  on Unix; equivalent guarantees on other platforms need review. Loaded
  configuration is validated. The crate has a narrow, stable role.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted for consistent XDG/platform path resolution.

### `ignore` 0.4 (locked: 0.4.33)

- **Purpose:** Walk project trees while applying `.gitignore`, hidden-file, and
  ignored-file policy for the explorer, file finder, and project search.
- **Alternatives considered:** `walkdir` plus a separate gitignore parser would
  recreate traversal and precedence logic. A custom walker is error-prone.
  Calling `ripgrep` would violate the built-in-search requirement and create a
  required external process.
- **Startup/performance:** Project traversal can be expensive, but it runs on a
  cancellable background thread and streams through a bounded channel. The
  crate itself does no work until a scan is requested. Scan limits and ignored
  directories are important for latency and memory control.
- **Portability/build:** Rust source, reusing the mature traversal code from the
  ripgrep project. Transitive filesystem helpers use Rust OS bindings; no
  bundled C/C++ compiler step is introduced.
- **Security/maintenance:** It parses repository-controlled ignore files and
  traverses potentially hostile directory trees. The runtime bounds emitted
  scan/search results, bounds files read for search, and supports cancellation;
  the current full-project walk does not have a universal depth or visited-entry
  cap. Symlink following is disabled by default. The crate is actively used by
  ripgrep.
- **License:** Unlicense OR MIT.

**Decision:** Accepted. Its correctness and performance are preferable to a
new ignore-rule implementation.

### `portable-pty` 0.9 (locked: 0.9.0)

- **Purpose:** Create, resize, read, write, and shut down the pseudo-terminal
  used by the explicitly toggled integrated shell.
- **Alternatives considered:** Ordinary child-process pipes do not provide
  terminal line discipline, job control, resize signals, or correct behavior
  for interactive programs. Maintaining Unix `openpty`/session setup and a
  separate Windows ConPTY adapter locally would duplicate sensitive OS code.
- **Startup/performance:** It performs no work during editor startup. A PTY and
  one reader thread are created only on the first terminal toggle; output moves
  through a bounded 64-chunk queue and is drained by the foreground runtime.
- **Portability/build:** Rust source with platform-specific Rust dependencies.
  Unix uses `libc`, `nix`, and `filedescriptor` to cross the OS ABI; Windows
  uses ConPTY bindings. It compiles no bundled C/C++ source.
- **Security/maintenance:** The terminal deliberately launches the user's
  `$SHELL` in the selected project root with the editor's environment and
  permissions, so it is not a sandbox. Launch requires an explicit command,
  arguments are not composed through a second shell, output is bounded before
  parsing, and dropping the owner kills the shell and closes the PTY.
- **License:** MIT.

**Decision:** Accepted because a real PTY is required for an interactive
terminal; its process and memory boundaries are kept outside the text model.

### `regex` 1 (locked: 1.13.1)

- **Purpose:** Regular-expression matching for line-oriented buffer searches
  and user-selected regex mode in project-wide text search.
- **Alternatives considered:** Literal search is implemented without it, but a
  custom regex engine is out of scope. Smaller regex crates generally support a
  narrower syntax; backtracking engines can expose worse worst-case behavior.
- **Startup/performance:** No work at editor startup. Project-search patterns
  compile and run on a worker over files capped by `max_file_bytes`.
  Normal-mode `/` and `?` compile and scan the active buffer synchronously;
  stdin ingestion and later editing are not currently capped by that file-open
  limit, so foreground search latency still needs measurement and hardening.
- **Portability/build:** Pure Rust and no native build step.
- **Security/maintenance:** Mature Rust project with a non-backtracking
  matching model. Invalid patterns become visible search errors. Project search
  has file/result bounds and cancellation, but an unusually large active buffer
  can still make synchronous local search expensive. Release maintenance must
  monitor advisories and memory-limit behavior.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted for the required regex search mode.

### `serde` 1 with `derive` (locked: 1.0.229)

- **Purpose:** Strongly typed serialization/deserialization for configuration,
  recovery/session records, and integration protocol data.
- **Alternatives considered:** Hand-written parsers would increase validation
  and compatibility risk. Format-specific derive systems would duplicate the
  data model.
- **Startup/performance:** Derive macros affect compile time only. Runtime
  deserialization is proportional to its input and avoids a general
  reflection/runtime system. Tool protocol data is byte-bounded; configuration
  and persisted-state reads still need explicit caps.
- **Portability/build:** Pure Rust. `serde_derive` is a build-time procedural
  macro and does not ship in the editor binary as a runtime.
- **Security/maintenance:** Very widely used and maintained. External tool
  protocol frames are size-bounded, config structs deny unknown fields, and
  schema versions guard persisted data. Config and persisted-state reads do not
  yet have explicit byte caps, so they remain resource-hardening work.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted as the common typed data boundary.

### `serde_json` 1 (locked: 1.0.151)

- **Purpose:** JSON-RPC/LSP messages and private state/recovery files.
- **Alternatives considered:** A hand-written JSON implementation would be a
  security and protocol-correctness risk. Faster specialized parsers often add
  unsafe code, architecture assumptions, or mutation requirements that do not
  benefit the current bounded message sizes.
- **Startup/performance:** No eager initialization. Parsing/encoding allocates
  in proportion to a message; the process layer caps frame sizes and uses
  bounded queues. Protocol parsing stays off the input/render path.
- **Portability/build:** Pure Rust and no native build step.
- **Security/maintenance:** Mature and broadly reviewed, but JSON from external
  processes is untrusted. Frame limits, object/type checks, malformed-message
  failure paths, and bounded logs reduce memory/availability risks. Private
  session/recovery JSON is version-checked but is not yet size-bounded on read.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted for interoperable, bounded protocols and state.

### `toml` 1 (locked: 1.1.5+spec-1.1.0)

- **Purpose:** Parse and merge the optional user and project configuration
  layers into strongly typed settings.
- **Alternatives considered:** `toml_edit` preserves formatting but is larger
  and unnecessary because the editor does not rewrite configuration. A custom
  parser would weaken TOML compatibility and validation.
- **Startup/performance:** `Config::load` always converts built-in defaults
  through a TOML value and deserializes the merged result; existing config files
  add synchronous reads and parses. This happens before interactive startup,
  so the round-trip needs measurement and unusually large config files should
  eventually gain an explicit byte limit.
- **Portability/build:** Pure Rust. Parser and writer dependencies add compile
  time and binary size but no native toolchain.
- **Security/maintenance:** Maintained in the `toml-rs` ecosystem. Unknown typed
  keys and unsupported schema versions are rejected, while untrusted project
  tool settings are discarded. Resource bounding for config input remains a
  hardening item.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted for declarative configuration without a scripting
runtime.

### `unicode-segmentation` 1 (locked: 1.13.3)

- **Purpose:** Extended-grapheme boundaries for cursor movement, edits, input,
  and selection so valid UTF-8 is never split mid-character.
- **Alternatives considered:** Scalar-value movement is visibly incorrect for
  combining marks and emoji. ICU4X would be substantially larger. Maintaining
  Unicode segmentation tables and rules locally is too risky.
- **Startup/performance:** No initialization. Operations scan the relevant
  string/line and are on the editing path; this is a conscious correctness
  tradeoff and a benchmark target for very long lines.
- **Portability/build:** Pure Rust, table-driven, and no OS/native dependency.
- **Security/maintenance:** Mature Unicode crate. The primary maintenance need
  is updating tables as Unicode evolves and regression-testing pathological
  combining sequences and long lines.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted; Unicode-safe editing is not practical without a tested
segmentation implementation.

### `unicode-width` 0.2 (locked: 0.2.2)

- **Purpose:** Convert displayed graphemes to terminal-cell widths for drawing
  and wide-cell continuation tracking.
- **Alternatives considered:** Treating every scalar as width one breaks CJK
  text and many emoji. Terminal probing is inconsistent and stateful; a custom
  width table would create a substantial maintenance burden.
- **Startup/performance:** No initialization; table lookup is performed while
  composing changed frames. Cost scales with rendered text and is included in
  the input-to-frame budget.
- **Portability/build:** Pure Rust and terminal-independent; no native build
  step.
- **Security/maintenance:** Small attack surface. Width rules vary among
  terminals and Unicode versions, so zero-width, wide, emoji, and ambiguous
  width cases need snapshot coverage.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted for terminal-column correctness.

### `vt100` 0.16 (locked: 0.16.2)

- **Purpose:** Parse the child terminal byte stream into a bounded screen and
  scrollback model, including colors, cursor state, alternate screens, and
  input modes such as application cursors and bracketed paste.
- **Alternatives considered:** Printing child escape sequences into the outer
  terminal would corrupt the editor's alternate screen and diff cache. A local
  ANSI parser would be a large, security-sensitive compatibility project.
- **Startup/performance:** The parser is initialized as lightweight model state;
  no process starts until requested. Parsing scales with PTY output, which is
  chunked through a bounded queue, and retained scrollback is capped at 10,000
  rows.
- **Portability/build:** Pure Rust using the `vte` state machine; no OS API or
  native build step.
- **Security/maintenance:** Child output is untrusted. Escape sequences are
  interpreted into cells instead of forwarded to the host terminal, and the
  canvas applies its existing control-character sanitization before output.
  Emulator-generated capability replies use a fixed-size queue. Mouse modes
  are observed but mouse input is not currently forwarded.
- **License:** MIT.

**Decision:** Accepted to isolate the host terminal from child-controlled
escape streams while preserving expected interactive terminal behavior.

## Development dependency

### `tempfile` 3 (locked: 3.27.0)

- **Purpose:** Isolated directories/files for application tests around LSP
  origin checks, sessions, external reloads, new-file/metadata behavior, and
  recovery-journal shutdown. Lower-level save conflict, permission, and symlink
  tests use the buffer module's own test-directory helper instead.
- **Alternatives considered:** A local test-directory helper would duplicate
  secure unique-name creation and cleanup. It would also be easier for parallel
  tests to interfere with one another.
- **Startup/performance:** Development/test only; absent from normal editor
  startup and runtime. Test creation performs expected filesystem and secure
  randomness calls.
- **Portability/build:** Rust source with transitive `rustix`/`getrandom` and OS
  ABI crates on relevant targets. It introduces no bundled C/C++ build and is
  not linked into release application code.
- **Security/maintenance:** Mature and specifically designed to avoid common
  temporary-file races. Tests must not mistake temporary permissions for the
  editor's production state-directory guarantees.
- **License:** MIT OR Apache-2.0.

**Decision:** Accepted for test isolation only.

## External executables (not linked dependencies)

`rust-analyzer` is an optional, separately installed process. It is launched
directly with an argument vector through `process`, never via a shell. Its
stdout/stderr and protocol frames are bounded, and failure must not disable
ordinary editing. It is never downloaded automatically. This integration does
not create a general-purpose plugin runtime.
The integrated terminal is separately user-triggered and directly launches the
user's shell through `portable-pty`; it is not used to implement tool commands.

## Review outcome

The direct dependency set satisfies Requirement 1 for the present MVP: it is
Rust-only at the source/package level, contains no scripting runtime, and has
documented OS ABI boundaries. The accepted costs requiring continued evidence
are Crossterm startup/render latency, Unicode work on long lines, background
filesystem scanning, PTY/VT output throughput, and the size/allocation limits
around parsed input.
