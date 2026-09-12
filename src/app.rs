// The public application error union is intentionally passed by value here;
// its platform-dependent size exceeds Clippy's threshold on Windows.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use mono::{
    CacheMode, CancellationToken, Error, OutputMode, PipelineExecution, ReleaseNotesTarget,
};

use crate::{
    CacheCommands, ChangelogCommands, Commands, ExecutionOptions, ReleaseCommands, UiFormat,
};

/// The application-level outcome of a parsed command.
///
/// Rendering is deliberately deferred to [`crate::render`]. `PreRendered` is a
/// compatibility boundary for library APIs that still return presentation text;
/// new command APIs should prefer the structured variants.
#[derive(Debug)]
pub(crate) enum CommandResult {
    Success { kind: &'static str, message: String },
    Check { project_root: PathBuf },
    PreRendered(String),
    EventsAlreadyEmitted,
}

struct ExecutionContext {
    output: OutputMode,
    root_ui: Option<UiFormat>,
    cancellation: CancellationToken,
    terminal: bool,
}

/// Execute a parsed command without writing to process-global output handles.
pub(crate) fn dispatch(
    root: PathBuf,
    output: OutputMode,
    root_ui: Option<UiFormat>,
    command: Option<Commands>,
    cancellation: CancellationToken,
    terminal: bool,
) -> Result<CommandResult, Error> {
    match command.unwrap_or_else(default_execution) {
        Commands::Init => run_init(&root),
        Commands::Run {
            pipeline,
            tasks,
            options,
        } => execute_pipeline(
            &root,
            pipeline.as_deref(),
            &tasks,
            options,
            ExecutionContext {
                output,
                root_ui,
                cancellation,
                terminal,
            },
        ),
        Commands::Task { tasks, options } => execute_pipeline(
            &root,
            None,
            &tasks,
            options,
            ExecutionContext {
                output,
                root_ui,
                cancellation,
                terminal,
            },
        ),
        Commands::Check => run_check(&root),
        Commands::List => run_list(&root, output),
        Commands::Plan { pipeline } => run_plan(&root, pipeline.as_deref(), output),
        Commands::Graph { pipeline } => run_graph(&root, pipeline.as_deref(), output),
        Commands::Cache { command } => run_cache(&root, command),
        Commands::Changelog { command } => run_changelog(&root, command),
        Commands::Release { command } => run_release(&root, command),
    }
}

/// The command a bare `mono` means: the default pipeline, no task selection,
/// the default execution options.
fn default_execution() -> Commands {
    Commands::Run {
        pipeline: None,
        tasks: Vec::new(),
        options: ExecutionOptions::default(),
    }
}

fn run_init(root: &Path) -> Result<CommandResult, Error> {
    let written = mono::init(root)?;
    Ok(CommandResult::Success {
        kind: "init",
        message: format!("initialized {}", written.display()),
    })
}

/// Run one pipeline or task selection with the parsed execution options.
fn execute_pipeline(
    root: &Path,
    pipeline: Option<&str>,
    tasks: &[String],
    options: ExecutionOptions,
    context: ExecutionContext,
) -> Result<CommandResult, Error> {
    let ui = options.ui.or(context.root_ui).unwrap_or(UiFormat::Auto);
    let result = mono::run_pipeline_with_mode(
        root,
        pipeline,
        tasks,
        options.dry_run,
        options.jobs,
        PipelineExecution {
            cache: cache_mode(options.no_cache, options.force),
            output: resolve_execution_output(context.output, ui, context.terminal),
            cancellation: context.cancellation,
        },
    )?;
    Ok(match result {
        Some(summary) => CommandResult::PreRendered(summary),
        None => CommandResult::EventsAlreadyEmitted,
    })
}

fn resolve_execution_output(output: OutputMode, ui: UiFormat, terminal: bool) -> OutputMode {
    if output == OutputMode::Json {
        return OutputMode::Json;
    }
    match ui {
        UiFormat::Stream => OutputMode::Stream,
        UiFormat::Tui | UiFormat::Auto => {
            if terminal {
                OutputMode::Tui
            } else {
                OutputMode::Stream
            }
        }
    }
}

fn run_check(root: &Path) -> Result<CommandResult, Error> {
    let project_root = mono::doctor(root)?;
    Ok(CommandResult::Check { project_root })
}

fn run_list(root: &Path, output: OutputMode) -> Result<CommandResult, Error> {
    Ok(CommandResult::PreRendered(mono::list_with_output(
        root, output,
    )?))
}

fn run_plan(
    root: &Path,
    pipeline: Option<&str>,
    output: OutputMode,
) -> Result<CommandResult, Error> {
    Ok(CommandResult::PreRendered(mono::plan_with_output(
        root,
        pipeline,
        &[],
        output,
    )?))
}

fn run_graph(
    root: &Path,
    pipeline: Option<&str>,
    output: OutputMode,
) -> Result<CommandResult, Error> {
    Ok(CommandResult::PreRendered(mono::graph_with_output(
        root,
        pipeline,
        &[],
        output,
    )?))
}

fn run_cache(root: &Path, command: CacheCommands) -> Result<CommandResult, Error> {
    match command {
        CacheCommands::Clean => Ok(CommandResult::Success {
            kind: "cache_clean",
            message: mono::clean_cache(root)?,
        }),
    }
}

fn run_changelog(root: &Path, command: ChangelogCommands) -> Result<CommandResult, Error> {
    let (kind, message) = match command {
        ChangelogCommands::Check { file } => (
            "changelog_check",
            mono::changelog_validate(&resolve_path(root, file))?,
        ),
        ChangelogCommands::Prepare {
            version,
            version_flag,
            date,
            from,
            to,
            pull_request_url,
            file,
        } => {
            let path = resolve_path(root, file);
            let version = version.or(version_flag);
            let message = match (from.as_deref(), to.as_deref()) {
                (Some(from), Some(to)) => mono::changelog_prepare_from_git_request(
                    &path,
                    version,
                    date.as_ref(),
                    from,
                    to,
                    pull_request_url.as_ref(),
                )?,
                (None, None) => mono::changelog_prepare_request(&path, version, date.as_ref())?,
                _ => unreachable!("clap requires --from and --to together"),
            };
            ("changelog_prepare", message)
        }
        ChangelogCommands::ReleaseNotes {
            version,
            version_flag,
            release_tag,
            file,
            output_file,
        } => {
            let version = version.or(version_flag);
            let target = ReleaseNotesTarget::from_requests(version, release_tag)?;
            (
                "changelog_release_notes",
                mono::changelog_release_notes(
                    &resolve_path(root, file),
                    target,
                    &resolve_path(root, output_file),
                )?,
            )
        }
    };
    Ok(CommandResult::Success { kind, message })
}

fn run_release(root: &Path, command: ReleaseCommands) -> Result<CommandResult, Error> {
    let (kind, message) = match command {
        ReleaseCommands::Source { identity } => (
            "release_source",
            mono::release_source(root, &identity.tag, &identity.commit)?,
        ),
        ReleaseCommands::Manifest {
            dist,
            expected,
            identity,
        } => {
            let directory = resolve_path(root, dist);
            let expected = expected.map(|path| resolve_path(root, path));
            (
                "release_manifest",
                mono::release_manifest(&directory, identity.resolve(), expected.as_deref())?,
            )
        }
        ReleaseCommands::Verify {
            dist,
            expected,
            identity,
        } => {
            let directory = resolve_path(root, dist);
            let expected = expected.map(|path| resolve_path(root, path));
            (
                "release_verify",
                mono::release_verify(&directory, identity.resolve(), expected.as_deref())?,
            )
        }
    };
    Ok(CommandResult::Success { kind, message })
}

fn cache_mode(no_cache: bool, force: bool) -> CacheMode {
    assert!(
        !(no_cache && force),
        "--no-cache and --force are mutually exclusive"
    );
    if no_cache {
        CacheMode::NoCache
    } else if force {
        CacheMode::Force
    } else {
        CacheMode::ReadWrite
    }
}

fn resolve_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_output_mode_is_resolved_from_output_ui_and_terminal() {
        use OutputMode::{Json, Stream, Terminal, Tui};

        assert_eq!(
            resolve_execution_output(Terminal, UiFormat::Auto, true),
            Tui
        );
        assert_eq!(
            resolve_execution_output(Terminal, UiFormat::Auto, false),
            Stream
        );
        assert_eq!(
            resolve_execution_output(Terminal, UiFormat::Stream, true),
            Stream
        );
        assert_eq!(resolve_execution_output(Json, UiFormat::Tui, true), Json);
    }

    #[test]
    fn cache_flags_resolve_to_one_mode() {
        assert!(matches!(cache_mode(false, false), CacheMode::ReadWrite));
        assert!(matches!(cache_mode(true, false), CacheMode::NoCache));
        assert!(matches!(cache_mode(false, true), CacheMode::Force));
    }

    #[test]
    #[should_panic(expected = "mutually exclusive")]
    fn conflicting_cache_flags_are_a_broken_invariant() {
        cache_mode(true, true);
    }

    #[test]
    fn relative_paths_are_rooted_and_absolute_paths_are_preserved() {
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_path(root, PathBuf::from("CHANGELOG.md")),
            PathBuf::from("/workspace/CHANGELOG.md")
        );
        assert_eq!(
            resolve_path(root, PathBuf::from("/tmp/notes.md")),
            PathBuf::from("/tmp/notes.md")
        );
    }
}
