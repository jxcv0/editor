//! Small, allocation-light command-line parser for the `editor` binary.
//!
//! Paths are deliberately kept as [`OsString`]s (inside [`PathBuf`]) so a
//! Unix path does not have to be valid UTF-8.  This module also deliberately
//! does not inspect the filesystem: a path is classified as a file or a
//! directory by startup orchestration after parsing.

use std::{
    env,
    ffi::{OsStr, OsString},
    fmt,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

/// The action requested by the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CliAction {
    /// Start the interactive editor.
    #[default]
    Run,
    /// Print [`HELP`] and exit successfully.
    Help,
    /// Print [`VERSION`] and exit successfully.
    Version,
}

/// A one-based cursor location supplied as `+LINE[:COLUMN]`.
///
/// Keeping the values non-zero makes it difficult for startup code to confuse
/// the user-facing location with the editor core's zero-based positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: NonZeroUsize,
    pub column: Option<NonZeroUsize>,
}

impl Position {
    pub const fn new(line: NonZeroUsize, column: Option<NonZeroUsize>) -> Self {
        Self { line, column }
    }

    /// Return the corresponding zero-based line index.
    pub const fn line_index(self) -> usize {
        self.line.get() - 1
    }

    /// Return the corresponding zero-based column index, when one was given.
    pub const fn column_index(self) -> Option<usize> {
        match self.column {
            Some(column) => Some(column.get() - 1),
            None => None,
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "+{}", self.line)?;
        if let Some(column) = self.column {
            write!(formatter, ":{column}")?;
        }
        Ok(())
    }
}

/// The source for an initial editor buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A file or directory path. Directory classification is intentionally
    /// deferred until after parsing.
    Path(PathBuf),
    /// Standard input, spelled `-` on the command line.
    Stdin,
}

/// One initial input together with an optional cursor location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub input: Input,
    pub position: Option<Position>,
}

impl Target {
    pub fn path(&self) -> Option<&Path> {
        match &self.input {
            Input::Path(path) => Some(path),
            Input::Stdin => None,
        }
    }

    pub const fn is_stdin(&self) -> bool {
        matches!(self.input, Input::Stdin)
    }
}

/// Fully parsed editor command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cli {
    pub action: CliAction,
    pub targets: Vec<Target>,
    pub no_session: bool,
}

impl Cli {
    /// Parse the current process command line.
    pub fn parse_env() -> Result<Self, CliError> {
        Self::parse_args(env::args_os().skip(1))
    }

    /// Parse arguments *after* the executable name.
    ///
    /// Options may be mixed with operands. `--` ends option and position
    /// parsing, allowing paths such as `--help` or `+12` to be opened.
    pub fn parse_args<I, S>(arguments: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut cli = Self::default();
        let mut parse_options = true;
        let mut pending_position: Option<(Position, OsString)> = None;
        let mut saw_stdin = false;

        for argument in arguments {
            let argument = argument.into();

            if parse_options && argument == OsStr::new("--") {
                parse_options = false;
                continue;
            }

            if parse_options {
                if argument == OsStr::new("--no-session") {
                    cli.no_session = true;
                    continue;
                }
                if argument == OsStr::new("--help") {
                    set_action(&mut cli.action, CliAction::Help)?;
                    continue;
                }
                if argument == OsStr::new("--version") {
                    set_action(&mut cli.action, CliAction::Version)?;
                    continue;
                }

                if let Some(text) = argument.to_str() {
                    if text.starts_with("--") {
                        return Err(CliError::UnknownOption(argument));
                    }
                    if text.starts_with('-') && text != "-" {
                        return Err(CliError::UnknownOption(argument));
                    }
                    if text.starts_with('+') {
                        if let Some((_, previous)) = pending_position {
                            return Err(CliError::PositionWithoutTarget(previous));
                        }
                        pending_position = Some((parse_position(text)?, argument));
                        continue;
                    }
                }
            }

            let input = if argument == OsStr::new("-") {
                if saw_stdin {
                    return Err(CliError::DuplicateStdin);
                }
                saw_stdin = true;
                Input::Stdin
            } else {
                Input::Path(PathBuf::from(argument))
            };
            let position = pending_position.take().map(|(position, _)| position);
            cli.targets.push(Target { input, position });
        }

        if let Some((_, argument)) = pending_position {
            return Err(CliError::PositionWithoutTarget(argument));
        }

        Ok(cli)
    }
}

fn set_action(action: &mut CliAction, requested: CliAction) -> Result<(), CliError> {
    if *action == CliAction::Run || *action == requested {
        *action = requested;
        Ok(())
    } else {
        Err(CliError::ConflictingActions {
            first: *action,
            second: requested,
        })
    }
}

fn parse_position(argument: &str) -> Result<Position, CliError> {
    let value = &argument[1..];
    let (line, column) = match value.split_once(':') {
        Some((line, column)) => (line, Some(column)),
        None => (value, None),
    };

    let line = parse_position_part(argument, PositionPart::Line, line)?;
    let column = column
        .map(|column| parse_position_part(argument, PositionPart::Column, column))
        .transpose()?;
    Ok(Position::new(line, column))
}

fn parse_position_part(
    argument: &str,
    part: PositionPart,
    value: &str,
) -> Result<NonZeroUsize, CliError> {
    if value.is_empty() {
        return Err(CliError::InvalidPosition {
            argument: argument.to_owned(),
            part,
            problem: PositionProblem::Missing,
        });
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CliError::InvalidPosition {
            argument: argument.to_owned(),
            part,
            problem: PositionProblem::NotDecimal,
        });
    }
    let number = value
        .parse::<usize>()
        .map_err(|_| CliError::InvalidPosition {
            argument: argument.to_owned(),
            part,
            problem: PositionProblem::TooLarge,
        })?;
    NonZeroUsize::new(number).ok_or_else(|| CliError::InvalidPosition {
        argument: argument.to_owned(),
        part,
        problem: PositionProblem::Zero,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionPart {
    Line,
    Column,
}

impl fmt::Display for PositionPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Line => formatter.write_str("line"),
            Self::Column => formatter.write_str("column"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionProblem {
    Missing,
    NotDecimal,
    Zero,
    TooLarge,
}

/// A deterministic command-line error suitable for printing to stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    UnknownOption(OsString),
    InvalidPosition {
        argument: String,
        part: PositionPart,
        problem: PositionProblem,
    },
    PositionWithoutTarget(OsString),
    DuplicateStdin,
    ConflictingActions {
        first: CliAction,
        second: CliAction,
    },
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(option) => write!(
                formatter,
                "unknown option {option:?}; use `--` before a path that begins with `-`"
            ),
            Self::InvalidPosition {
                argument,
                part,
                problem,
            } => {
                write!(formatter, "invalid position {argument:?}: {part} ")?;
                match problem {
                    PositionProblem::Missing => formatter.write_str("is missing")?,
                    PositionProblem::NotDecimal => {
                        formatter.write_str("must contain only decimal digits")?
                    }
                    PositionProblem::Zero => formatter.write_str("must be greater than zero")?,
                    PositionProblem::TooLarge => {
                        formatter.write_str("is too large for this platform")?
                    }
                }
                formatter.write_str(" (expected `+LINE[:COLUMN] FILE`)")
            }
            Self::PositionWithoutTarget(position) => write!(
                formatter,
                "position {position:?} must be followed by a file path"
            ),
            Self::DuplicateStdin => {
                formatter.write_str("standard input (`-`) may only be specified once")
            }
            Self::ConflictingActions { first, second } => write!(
                formatter,
                "`--{}` cannot be combined with `--{}`",
                first.option_name(),
                second.option_name()
            ),
        }
    }
}

impl std::error::Error for CliError {}

impl CliAction {
    const fn option_name(self) -> &'static str {
        match self {
            Self::Run => "",
            Self::Help => "help",
            Self::Version => "version",
        }
    }
}

/// Displayable static help, intended for `println!("{HELP}")`.
#[derive(Debug, Clone, Copy)]
pub struct HelpText;

pub const HELP: HelpText = HelpText;

impl fmt::Display for HelpText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "A fast, modal terminal editor\n\n\
             Usage:\n\
               editor [OPTIONS] [PATH ...]\n\
               editor [OPTIONS] +LINE[:COLUMN] FILE\n\n\
             Arguments:\n\
               PATH ...       Files to open; a directory selects the project root\n\
               -              Read a scratch buffer from standard input\n\n\
             Options:\n\
               --no-session   Do not restore the project's previous session\n\
               --help         Print this help and exit\n\
               --version      Print version information and exit\n\
               --             Stop parsing options and cursor positions",
        )
    }
}

/// Displayable package version, intended for `println!("{VERSION}")`.
#[derive(Debug, Clone, Copy)]
pub struct VersionText;

pub const VERSION: VersionText = VersionText;

impl fmt::Display for VersionText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}",
            option_env!("CARGO_PKG_NAME").unwrap_or("editor"),
            option_env!("CARGO_PKG_VERSION").unwrap_or("unknown")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<const N: usize>(arguments: [&str; N]) -> Result<Cli, CliError> {
        Cli::parse_args(arguments)
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[test]
    fn empty_arguments_start_a_session_enabled_scratch_editor() {
        assert_eq!(parse([]).unwrap(), Cli::default());
    }

    #[test]
    fn parses_files_directories_and_stdin_in_order() {
        let cli = parse(["src/main.rs", "project", "-"]).unwrap();
        assert_eq!(
            cli.targets,
            vec![
                Target {
                    input: Input::Path("src/main.rs".into()),
                    position: None,
                },
                Target {
                    input: Input::Path("project".into()),
                    position: None,
                },
                Target {
                    input: Input::Stdin,
                    position: None,
                },
            ]
        );
    }

    #[test]
    fn attaches_positions_only_to_the_following_target() {
        let cli = parse(["+12:7", "one.rs", "two.rs", "+3", "three.rs"]).unwrap();
        assert_eq!(
            cli.targets,
            vec![
                Target {
                    input: Input::Path("one.rs".into()),
                    position: Some(Position::new(non_zero(12), Some(non_zero(7)))),
                },
                Target {
                    input: Input::Path("two.rs".into()),
                    position: None,
                },
                Target {
                    input: Input::Path("three.rs".into()),
                    position: Some(Position::new(non_zero(3), None)),
                },
            ]
        );
        assert_eq!(cli.targets[0].position.unwrap().line_index(), 11);
        assert_eq!(cli.targets[0].position.unwrap().column_index(), Some(6));
    }

    #[test]
    fn parses_options_among_operands() {
        let cli = parse(["one.rs", "--no-session", "two.rs"]).unwrap();
        assert!(cli.no_session);
        assert_eq!(cli.action, CliAction::Run);
        assert_eq!(cli.targets.len(), 2);
    }

    #[test]
    fn help_and_version_are_typed_actions() {
        assert_eq!(parse(["--help"]).unwrap().action, CliAction::Help);
        assert_eq!(parse(["--version"]).unwrap().action, CliAction::Version);
        assert!(matches!(
            parse(["--help", "--version"]),
            Err(CliError::ConflictingActions { .. })
        ));
    }

    #[test]
    fn end_of_options_makes_option_and_position_looking_values_paths() {
        let cli = parse(["--", "--help", "+12", "-"]).unwrap();
        assert_eq!(cli.action, CliAction::Run);
        assert_eq!(
            cli.targets,
            vec![
                Target {
                    input: Input::Path("--help".into()),
                    position: None,
                },
                Target {
                    input: Input::Path("+12".into()),
                    position: None,
                },
                Target {
                    input: Input::Stdin,
                    position: None,
                },
            ]
        );
    }

    #[test]
    fn options_may_appear_between_a_position_and_its_target() {
        let cli = parse(["+4:2", "--no-session", "file.rs"]).unwrap();
        assert!(cli.no_session);
        assert_eq!(
            cli.targets[0].position,
            Some(Position::new(non_zero(4), Some(non_zero(2))))
        );
    }

    #[test]
    fn rejects_unknown_options_with_an_escape_hint() {
        let error = parse(["--wat"]).unwrap_err();
        assert_eq!(error, CliError::UnknownOption("--wat".into()));
        let message = error.to_string();
        assert!(message.contains("unknown option"));
        assert!(message.contains("use `--`"));
    }

    #[test]
    fn rejects_missing_or_repeated_position_targets() {
        assert_eq!(
            parse(["+9"]).unwrap_err(),
            CliError::PositionWithoutTarget("+9".into())
        );
        assert_eq!(
            parse(["+9", "+10", "file.rs"]).unwrap_err(),
            CliError::PositionWithoutTarget("+9".into())
        );
    }

    #[test]
    fn rejects_malformed_or_zero_positions() {
        for argument in ["+", "+:2", "+2:", "+hello", "+1:2:3", "+0", "+1:0"] {
            assert!(
                matches!(parse([argument]), Err(CliError::InvalidPosition { .. })),
                "{argument} unexpectedly parsed"
            );
        }
    }

    #[test]
    fn rejects_duplicate_standard_input() {
        assert_eq!(parse(["-", "-"]).unwrap_err(), CliError::DuplicateStdin);
    }

    #[test]
    fn position_and_help_have_useful_display_text() {
        let position = Position::new(non_zero(42), Some(non_zero(6)));
        assert_eq!(position.to_string(), "+42:6");
        assert!(HELP.to_string().contains("+LINE[:COLUMN] FILE"));
        assert!(HELP.to_string().contains("--no-session"));
        assert!(VERSION.to_string().starts_with("editor "));
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let raw = b"bad-\xff-name".to_vec();
        let cli = Cli::parse_args([OsString::from_vec(raw.clone())]).unwrap();
        let Input::Path(path) = &cli.targets[0].input else {
            panic!("path became another input kind");
        };
        assert_eq!(path.as_os_str().as_bytes(), raw);
    }
}
