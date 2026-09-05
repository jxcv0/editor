//! Versioned, declarative configuration.  There is deliberately no evaluation
//! hook here: key bindings can only name commands from the typed registry.

use std::{
    collections::BTreeMap,
    env, fmt, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub editor: EditorConfig,
    pub ui: UiConfig,
    pub tools: ToolsConfig,
    pub limits: LimitsConfig,
    pub keymap: BTreeMap<String, String>,
    #[serde(skip)]
    pub sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    pub tab_width: usize,
    pub insert_spaces: bool,
    pub format_on_save: bool,
    pub persistent_undo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub relative_numbers: bool,
    pub true_color: bool,
    pub explorer_width: u16,
    pub show_hidden: bool,
    pub show_ignored: bool,
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    pub background: String,
    pub foreground: String,
    pub muted: String,
    pub accent: String,
    pub status: String,
    pub error: String,
    pub warning: String,
    pub info: String,
    pub selection: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub rust_analyzer: ToolConfig,
    pub codex_watch: ToolConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolConfig {
    pub path: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_file_bytes: u64,
    pub large_file_bytes: u64,
    pub message_history: usize,
    pub undo_steps: usize,
    /// Maximum retained undo payload per buffer; zero disables retained edits.
    pub undo_bytes: usize,
    pub search_results: usize,
    pub tool_message_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            editor: EditorConfig::default(),
            ui: UiConfig::default(),
            tools: ToolsConfig::default(),
            limits: LimitsConfig::default(),
            keymap: BTreeMap::new(),
            sources: Vec::new(),
        }
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_width: 4,
            insert_spaces: true,
            format_on_save: false,
            persistent_undo: true,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            relative_numbers: true,
            true_color: true,
            explorer_width: 30,
            show_hidden: false,
            show_ignored: false,
            theme: ThemeConfig::default(),
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            background: "#101619".into(),
            foreground: "#dce4e3".into(),
            muted: "#77858a".into(),
            accent: "#8bd5b6".into(),
            status: "#1b252b".into(),
            error: "#f7768e".into(),
            warning: "#e0af68".into(),
            info: "#8bbfd8".into(),
            selection: "#2b4548".into(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            rust_analyzer: ToolConfig {
                path: "rust-analyzer".into(),
                args: Vec::new(),
            },
            codex_watch: ToolConfig {
                path: "codex-watch".into(),
                args: vec!["--json-events".into()],
            },
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 100 * 1024 * 1024,
            large_file_bytes: 10 * 1024 * 1024,
            message_history: 256,
            undo_steps: 1_000,
            undo_bytes: 64 * 1024 * 1024,
            search_results: 2_000,
            tool_message_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub detail: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.detail)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn user_path() -> Option<PathBuf> {
        if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(dir).join("editor/config.toml"));
        }
        dirs::config_dir().map(|p| p.join("editor/config.toml"))
    }

    /// Load defaults, user config, then project config.  Tool commands from a
    /// project are ignored until that project has explicitly been trusted.
    pub fn load(project_root: Option<&Path>, project_trusted: bool) -> Result<Self, ConfigError> {
        let mut value = toml::Value::try_from(Self::default()).expect("defaults serialize");
        let mut sources = Vec::new();
        if let Some(user) = Self::user_path().filter(|p| p.exists()) {
            merge_value(&mut value, Self::read_layer(&user)?);
            sources.push(user);
        }
        if let Some(path) = project_root
            .map(|p| p.join(".editor.toml"))
            .filter(|p| p.exists())
        {
            let mut layer = Self::read_layer(&path)?;
            if !project_trusted && let Some(table) = layer.as_table_mut() {
                table.remove("tools");
            }
            merge_value(&mut value, layer);
            sources.push(path);
        }
        let mut config: Config = value.try_into().map_err(|e: toml::de::Error| ConfigError {
            path: sources
                .last()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("<built-in>")),
            detail: e.to_string(),
        })?;
        config.sources = sources;
        config.validate()?;
        Ok(config)
    }

    fn read_layer(path: &Path) -> Result<toml::Value, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| ConfigError {
            path: path.into(),
            detail: e.to_string(),
        })?;
        toml::from_str(&text).map_err(|e| ConfigError {
            path: path.into(),
            detail: e.to_string(),
        })
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let path = self
            .sources
            .last()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("<built-in>"));
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError {
                path,
                detail: format!(
                    "unsupported schema_version {}; expected {}",
                    self.schema_version, CONFIG_SCHEMA_VERSION
                ),
            });
        }
        if !(1..=16).contains(&self.editor.tab_width) {
            return Err(ConfigError {
                path,
                detail: "editor.tab_width must be between 1 and 16".into(),
            });
        }
        if !(16..=120).contains(&self.ui.explorer_width) {
            return Err(ConfigError {
                path,
                detail: "ui.explorer_width must be between 16 and 120".into(),
            });
        }
        for color in [
            &self.ui.theme.background,
            &self.ui.theme.foreground,
            &self.ui.theme.muted,
            &self.ui.theme.accent,
            &self.ui.theme.status,
            &self.ui.theme.error,
            &self.ui.theme.warning,
            &self.ui.theme.info,
            &self.ui.theme.selection,
        ] {
            if parse_hex_color(color).is_none() {
                return Err(ConfigError {
                    path,
                    detail: format!("invalid theme color {color:?}; use #rrggbb"),
                });
            }
        }
        for (binding, command) in &self.keymap {
            if binding.trim().is_empty() {
                return Err(ConfigError {
                    path,
                    detail: "keymap bindings cannot be empty".into(),
                });
            }
            if crate::command::id_by_name(command).is_none() {
                return Err(ConfigError {
                    path,
                    detail: format!("keymap {binding:?} names unknown typed command {command:?}"),
                });
            }
        }
        Ok(())
    }
}

fn merge_value(lower: &mut toml::Value, upper: toml::Value) {
    match (lower, upper) {
        (toml::Value::Table(lower), toml::Value::Table(upper)) => {
            for (key, value) in upper {
                if let Some(previous) = lower.get_mut(&key) {
                    merge_value(previous, value);
                } else {
                    lower.insert(key, value);
                }
            }
        }
        (lower, upper) => *lower = upper,
    }
}

pub fn parse_hex_color(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_are_strict() {
        assert_eq!(parse_hex_color("#7aa2f7"), Some((122, 162, 247)));
        assert_eq!(parse_hex_color("#ABCDEF"), Some((171, 205, 239)));
        for invalid in [
            "blue", "#12345", "#1234567", "#gg0000", "#00gg00", "#0000gg", "#aéabc", "#aaaéb",
            "#aaaaé",
        ] {
            assert_eq!(parse_hex_color(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn unknown_keys_fail() {
        let result: Result<Config, _> = toml::from_str("schema_version = 1\nmagic = true\n");
        assert!(result.is_err());
    }

    #[test]
    fn keymaps_accept_only_registered_typed_commands() {
        let mut config = Config::default();
        config.keymap.insert("K".into(), "code.hover".into());
        assert!(config.validate().is_ok());

        config.keymap.insert("X".into(), "shell.arbitrary".into());
        let error = config.validate().unwrap_err();
        assert!(error.detail.contains("unknown typed command"));
    }
}
