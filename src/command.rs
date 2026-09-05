//! Declarative command registry shared by the editor and external tools.

use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    Hover,
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
    References,
    Completion,
    SignatureHelp,
    RustAnalyzerRestart,
    FindFiles,
    ProjectGrep,
    ExplorerToggle,
    TerminalToggle,
    BufferSwitch,
    BufferClose,
    BufferNext,
    BufferPrevious,
    CodeAction,
    Format,
    Rename,
    RecentFiles,
    DocumentSymbols,
    WorkspaceSymbols,
    Messages,
    BufferDiagnostics,
    WorkspaceDiagnostics,
    ToggleInlayHints,
    SplitBelow,
    SplitRight,
    ClosePane,
    OnlyPane,
    CodexToggle,
    CodexStatus,
    CodexRestart,
    CodexRunOnce,
    CodexLogs,
    CodexDryRun,
    CodexWorkspaceWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Editor,
    RustAnalyzer,
    CodexWatch,
}

#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub id: CommandId,
    pub name: &'static str,
    pub sequence: &'static str,
    pub description: &'static str,
    pub source: CommandSource,
}

pub const COMMANDS: &[Command] = &[
    Command {
        id: CommandId::FindFiles,
        name: "files.find",
        sequence: " ",
        description: "Find project files",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::ProjectGrep,
        name: "search.grep",
        sequence: "/",
        description: "Search project text",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::ExplorerToggle,
        name: "explorer.toggle",
        sequence: "e",
        description: "Explorer",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::TerminalToggle,
        name: "terminal.toggle",
        sequence: "t",
        description: "Terminal",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::BufferSwitch,
        name: "buffer.switch",
        sequence: "bb",
        description: "Switch buffer",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::BufferClose,
        name: "buffer.close",
        sequence: "bd",
        description: "Close buffer",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::BufferNext,
        name: "buffer.next",
        sequence: "bn",
        description: "Next buffer",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::BufferPrevious,
        name: "buffer.previous",
        sequence: "bp",
        description: "Previous buffer",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::CodeAction,
        name: "code.action",
        sequence: "ca",
        description: "Code action",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::Format,
        name: "code.format",
        sequence: "cf",
        description: "Format buffer",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::Rename,
        name: "code.rename",
        sequence: "cr",
        description: "Rename symbol",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::RustAnalyzerRestart,
        name: "code.restart_analyzer",
        sequence: "cR",
        description: "Restart rust-analyzer",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::FindFiles,
        name: "files.find",
        sequence: "ff",
        description: "Find project files",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::RecentFiles,
        name: "files.recent",
        sequence: "fr",
        description: "Recent files",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::ProjectGrep,
        name: "search.grep",
        sequence: "sg",
        description: "Grep project",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::DocumentSymbols,
        name: "search.document_symbols",
        sequence: "ss",
        description: "Document symbols",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::WorkspaceSymbols,
        name: "search.workspace_symbols",
        sequence: "sS",
        description: "Workspace symbols",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::Messages,
        name: "search.messages",
        sequence: "sm",
        description: "Message history",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::BufferDiagnostics,
        name: "diagnostics.buffer",
        sequence: "xX",
        description: "Buffer diagnostics",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::WorkspaceDiagnostics,
        name: "diagnostics.workspace",
        sequence: "xx",
        description: "Workspace diagnostics",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::ToggleInlayHints,
        name: "ui.inlay_hints",
        sequence: "uh",
        description: "Toggle inlay hints",
        source: CommandSource::RustAnalyzer,
    },
    Command {
        id: CommandId::SplitBelow,
        name: "window.split_below",
        sequence: "-",
        description: "Split below",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::SplitRight,
        name: "window.split_right",
        sequence: "|",
        description: "Split right",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::ClosePane,
        name: "window.close",
        sequence: "wd",
        description: "Close pane",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::OnlyPane,
        name: "window.only",
        sequence: "wo",
        description: "Keep only pane",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::SplitBelow,
        name: "window.split_below",
        sequence: "w-",
        description: "Split below",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::SplitRight,
        name: "window.split_right",
        sequence: "w|",
        description: "Split right",
        source: CommandSource::Editor,
    },
    Command {
        id: CommandId::CodexToggle,
        name: "codex.toggle",
        sequence: "at",
        description: "Start / stop",
        source: CommandSource::CodexWatch,
    },
    Command {
        id: CommandId::CodexStatus,
        name: "codex.status",
        sequence: "as",
        description: "Show status",
        source: CommandSource::CodexWatch,
    },
    Command {
        id: CommandId::CodexRestart,
        name: "codex.restart",
        sequence: "ar",
        description: "Restart",
        source: CommandSource::CodexWatch,
    },
    Command {
        id: CommandId::CodexRunOnce,
        name: "codex.run_once",
        sequence: "ao",
        description: "Run once",
        source: CommandSource::CodexWatch,
    },
    Command {
        id: CommandId::CodexLogs,
        name: "codex.logs",
        sequence: "al",
        description: "Show logs",
        source: CommandSource::CodexWatch,
    },
    Command {
        id: CommandId::CodexDryRun,
        name: "codex.dry_run",
        sequence: "ad",
        description: "Use dry-run mode",
        source: CommandSource::CodexWatch,
    },
    Command {
        id: CommandId::CodexWorkspaceWrite,
        name: "codex.workspace_write",
        sequence: "aw",
        description: "Use workspace-write mode",
        source: CommandSource::CodexWatch,
    },
];

pub fn by_sequence(sequence: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.sequence == sequence)
}

pub fn by_name(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// Resolve any typed command name, including non-leader commands whose
/// default keys live in the modal grammar.
pub fn id_by_name(name: &str) -> Option<CommandId> {
    by_name(name).map(|command| command.id).or(match name {
        "code.hover" => Some(CommandId::Hover),
        "code.definition" => Some(CommandId::Definition),
        "code.declaration" => Some(CommandId::Declaration),
        "code.type_definition" => Some(CommandId::TypeDefinition),
        "code.implementation" => Some(CommandId::Implementation),
        "code.references" => Some(CommandId::References),
        "code.completion" => Some(CommandId::Completion),
        "code.signature_help" => Some(CommandId::SignatureHelp),
        _ => None,
    })
}

pub fn has_prefix(prefix: &str) -> bool {
    COMMANDS
        .iter()
        .any(|command| command.sequence.starts_with(prefix))
}

#[derive(Debug, Clone, Copy)]
pub struct MenuEntry {
    pub key: char,
    pub label: &'static str,
    pub group: bool,
}

pub fn menu_entries(prefix: &str) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for command in COMMANDS {
        let Some(rest) = command.sequence.strip_prefix(prefix) else {
            continue;
        };
        let Some(key) = rest.chars().next() else {
            continue;
        };
        let group = rest.chars().count() > 1;
        let label = if group {
            match (prefix, key) {
                ("", 'a') => "codex-watch",
                ("", 'b') => "buffers",
                ("", 'c') => "code",
                ("", 'f') => "files",
                ("", 's') => "search",
                ("", 'u') => "ui",
                ("", 'w') => "windows",
                ("", 'x') => "diagnostics",
                _ => "more",
            }
        } else {
            command.description
        };
        if !entries.iter().any(|entry: &MenuEntry| entry.key == key) {
            entries.push(MenuEntry { key, label, group });
        }
    }
    entries.sort_by_key(|entry| entry.key);
    entries
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = COMMANDS
            .iter()
            .find(|c| c.id == *self)
            .map(|c| c.name)
            .unwrap_or(match self {
                Self::Hover => "code.hover",
                Self::Definition => "code.definition",
                Self::Declaration => "code.declaration",
                Self::TypeDefinition => "code.type_definition",
                Self::Implementation => "code.implementation",
                Self::References => "code.references",
                Self::Completion => "code.completion",
                Self::SignatureHelp => "code.signature_help",
                _ => "unknown",
            });
        f.write_str(name)
    }
}

impl FromStr for CommandId {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id_by_name(s).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hierarchy_is_reachable() {
        assert_eq!(by_sequence("bb").unwrap().id, CommandId::BufferSwitch);
        assert!(has_prefix("b"));
        assert!(
            menu_entries("")
                .iter()
                .any(|entry| entry.key == 'b' && entry.group)
        );
        assert_eq!(id_by_name("code.hover"), Some(CommandId::Hover));
        assert_eq!(by_sequence("t").unwrap().id, CommandId::TerminalToggle);
        assert_eq!(
            by_sequence("cR").map(|command| command.id),
            Some(CommandId::RustAnalyzerRestart)
        );
    }
}
