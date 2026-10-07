use std::{
    env,
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
};

use editor::{
    app::Runtime,
    buffer::Pos,
    cli::{Cli, CliAction, HELP, Input, VERSION},
    config::Config,
    editor::{Editor, Focus},
    project,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("editor: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse_env()?;
    match cli.action {
        CliAction::Help => {
            println!("{HELP}");
            return Ok(());
        }
        CliAction::Version => {
            println!("{VERSION}");
            return Ok(());
        }
        CliAction::Run => {}
    }

    #[cfg(not(feature = "gui"))]
    if cli.gui {
        return Err(
            "this build has no GUI; rebuild with cargo build --release --features gui".into(),
        );
    }
    let working_directory = env::current_dir()?;
    let explicit_directory = cli
        .targets
        .iter()
        .filter_map(|target| target.path())
        .find(|path| path.is_dir())
        .map(Path::to_owned);
    let discovery_start = explicit_directory
        .as_deref()
        .or_else(|| cli.targets.iter().find_map(|target| target.path()))
        .unwrap_or(&working_directory);
    let project_root = project::discover_project_root(discovery_start)
        .unwrap_or_else(|_| working_directory.clone());
    let config = Config::load(Some(&project_root), false)?;
    let mut editor = Editor::new(config, project_root);

    if explicit_directory.is_some() {
        editor.explorer.open = true;
        editor.focus = Focus::Explorer;
    }

    let mut stdin_text = None;
    let mut first_file: Option<(PathBuf, Option<editor::cli::Position>)> = None;
    let has_explicit_files = cli
        .targets
        .iter()
        .any(|target| target.path().is_some_and(|path| !path.is_dir()));
    for target in &cli.targets {
        match &target.input {
            Input::Path(path) if path.is_dir() => {}
            Input::Path(path) => {
                if first_file.is_none() {
                    first_file = Some((path.clone(), target.position));
                }
                if let Err(error) = editor.open_path(path) {
                    editor.message(format!("Could not open {}: {error}", path.display()));
                }
            }
            Input::Stdin => {
                if stdin_text.is_none() {
                    let mut bytes = Vec::new();
                    io::stdin().read_to_end(&mut bytes)?;
                    stdin_text = Some(String::from_utf8(bytes).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "standard input is not UTF-8 (valid through byte {})",
                                error.utf8_error().valid_up_to()
                            ),
                        )
                    })?);
                }
                editor.open_scratch_text("[stdin]", stdin_text.as_deref().unwrap_or(""));
            }
        }
    }

    editor.discard_initial_scratch();
    if let Some((path, position)) = first_file
        && editor.open_path(&path).is_ok()
        && let Some(position) = position
    {
        let line = position
            .line_index()
            .min(editor.active_buffer().line_count().saturating_sub(1));
        let grapheme = position
            .column_index()
            .unwrap_or(0)
            .min(editor.active_buffer().grapheme_count(line).unwrap_or(0));
        editor.active_pane_mut().cursor =
            editor.active_buffer().clamp_pos(Pos::new(line, grapheme));
    }

    // Ensure a supplied non-existent parent is reported before entering the
    // alternate screen, while preserving errors already retained in messages.
    if let Some(parent) = first_file_parent(&cli)
        && !parent.exists()
    {
        editor.message(format!(
            "Parent directory does not exist: {}",
            parent.display()
        ));
    }

    let restore_session = !cli.no_session && !has_explicit_files && stdin_text.is_none();
    let runtime = Runtime::new(editor, restore_session)?;
    #[cfg(feature = "gui")]
    if cli.gui {
        return editor::gui::run(runtime);
    }
    runtime.run()?;
    Ok(())
}

fn first_file_parent(cli: &Cli) -> Option<&Path> {
    cli.targets
        .iter()
        .filter_map(|target| target.path())
        .find(|path| !path.is_dir() && !path.exists())
        .and_then(Path::parent)
        .filter(|path| !path.as_os_str().is_empty())
}
