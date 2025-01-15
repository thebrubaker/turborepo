use std::{
    backtrace::Backtrace,
    env,
    ffi::OsString,
    fmt::{self, Display},
    io, mem, process,
};

use biome_deserialize_macros::Deserializable;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{
    builder::NonEmptyStringValueParser, ArgAction, ArgGroup, CommandFactory, Parser, Subcommand,
    ValueEnum,
};
use clap_complete::{generate, Shell};
pub use error::Error;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, log::warn};
use turbopath::AbsoluteSystemPathBuf;
use turborepo_api_client::AnonAPIClient;
use turborepo_repository::inference::{RepoMode, RepoState};
use turborepo_telemetry::{
    events::{command::CommandEventBuilder, generic::GenericEventBuilder, EventBuilder, EventType},
    init_telemetry, track_usage, TelemetryHandle,
};
use turborepo_ui::{ColorConfig, GREY};

use crate::{
    cli::error::print_potential_tasks,
    commands::{
        bin, config, daemon, generate, info, link, login, logout, ls, prune, query, run, scan,
        telemetry, unlink, CommandBase,
    },
    get_version,
    run::watch::WatchClient,
    shim::TurboState,
    tracing::TurboSubscriber,
    turbo_json::UIMode,
};

mod error;
// Global turbo sets this environment variable to its cwd so that local
// turbo can use it for package inference.
pub const INVOCATION_DIR_ENV_VAR: &str = "TURBO_INVOCATION_DIR";

// Default value for the --cache-workers argument
const DEFAULT_NUM_WORKERS: u32 = 10;
const SUPPORTED_GRAPH_FILE_EXTENSIONS: [&str; 8] =
    ["svg", "png", "jpg", "pdf", "json", "html", "mermaid", "dot"];

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Deserializable, Serialize)]
pub enum OutputLogsMode {
    #[serde(rename = "full")]
    Full,
    #[serde(rename = "none")]
    None,
    #[serde(rename = "hash-only")]
    HashOnly,
    #[serde(rename = "new-only")]
    NewOnly,
    #[serde(rename = "errors-only")]
    ErrorsOnly,
}

impl Default for OutputLogsMode {
    fn default() -> Self {
        Self::Full
    }
}

impl Display for OutputLogsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OutputLogsMode::Full => "full",
            OutputLogsMode::None => "none",
            OutputLogsMode::HashOnly => "hash-only",
            OutputLogsMode::NewOnly => "new-only",
            OutputLogsMode::ErrorsOnly => "errors-only",
        })
    }
}

impl From<OutputLogsMode> for turborepo_ui::tui::event::OutputLogs {
    fn from(value: OutputLogsMode) -> Self {
        match value {
            OutputLogsMode::Full => turborepo_ui::tui::event::OutputLogs::Full,
            OutputLogsMode::None => turborepo_ui::tui::event::OutputLogs::None,
            OutputLogsMode::HashOnly => turborepo_ui::tui::event::OutputLogs::HashOnly,
            OutputLogsMode::NewOnly => turborepo_ui::tui::event::OutputLogs::NewOnly,
            OutputLogsMode::ErrorsOnly => turborepo_ui::tui::event::OutputLogs::ErrorsOnly,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, ValueEnum, Deserialize, Eq)]
pub enum LogOrder {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "stream")]
    Stream,
    #[serde(rename = "grouped")]
    Grouped,
}

impl Default for LogOrder {
    fn default() -> Self {
        Self::Auto
    }
}

impl Display for LogOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LogOrder::Auto => "auto",
            LogOrder::Stream => "stream",
            LogOrder::Grouped => "grouped",
        })
    }
}

impl LogOrder {
    pub fn compatible_with_tui(&self) -> bool {
        // If the user requested a specific order to the logs, then this isn't
        // compatible with the TUI and means we cannot use it.
        matches!(self, Self::Auto)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, ValueEnum)]
pub enum DryRunMode {
    Text,
    Json,
}

impl Display for DryRunMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DryRunMode::Text => "text",
            DryRunMode::Json => "json",
        })
    }
}

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Serialize, ValueEnum, Deserialize, Eq, Deserializable,
)]
#[serde(rename_all = "lowercase")]
pub enum EnvMode {
    Loose,
    #[default]
    Strict,
}

impl fmt::Display for EnvMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EnvMode::Loose => "loose",
            EnvMode::Strict => "strict",
        })
    }
}

/// The parsed arguments from the command line. In general we should avoid using
/// or mutating this directly, and instead use the fully canonicalized `Opts`
/// struct.
#[derive(Parser, Clone, Default, Debug, PartialEq)]
#[clap(author, about = "The build system that makes ship happen", long_about = None)]
#[clap(disable_help_subcommand = true)]
#[clap(disable_version_flag = true)]
#[clap(arg_required_else_help = true)]
#[command(name = "turbo")]
pub struct Args {
    #[clap(long, global = true)]
    pub version: bool,
    #[clap(long, global = true)]
    /// Skip any attempts to infer which version of Turbo the project is
    /// configured to use
    pub skip_infer: bool,
    /// Disable the turbo update notification
    #[clap(long, global = true)]
    pub no_update_notifier: bool,
    /// Override the endpoint for API calls
    #[clap(long, global = true, value_parser)]
    pub api: Option<String>,
    /// Force color usage in the terminal
    #[clap(long, global = true)]
    pub color: bool,
    /// The directory in which to run turbo
    #[clap(long, global = true, value_parser)]
    pub cwd: Option<Utf8PathBuf>,
    /// Specify a file to save a pprof heap profile
    #[clap(long, global = true, value_parser)]
    pub heap: Option<String>,
    /// Specify whether to use the streaming UI or TUI
    #[clap(long, global = true, value_enum)]
    pub ui: Option<UIMode>,
    /// Override the login endpoint
    #[clap(long, global = true, value_parser)]
    pub login: Option<String>,
    /// Suppress color usage in the terminal
    #[clap(long, global = true)]
    pub no_color: bool,
    /// When enabled, turbo will precede HTTP requests with an OPTIONS request
    /// for authorization
    #[clap(long, global = true)]
    pub preflight: bool,
    /// Set a timeout for all HTTP requests.
    #[clap(long, value_name = "TIMEOUT", global = true, value_parser)]
    pub remote_cache_timeout: Option<u64>,
    /// Set the team slug for API calls
    #[clap(long, global = true, value_parser)]
    pub team: Option<String>,
    /// Set the auth token for API calls
    #[clap(long, global = true, value_parser)]
    pub token: Option<String>,
    /// Specify a file to save a pprof trace
    #[clap(long, global = true, value_parser)]
    pub trace: Option<String>,
    /// verbosity
    #[clap(flatten)]
    pub verbosity: Verbosity,
    /// Force a check for a new version of turbo
    #[clap(long, global = true, hide = true)]
    pub check_for_update: bool,
    #[clap(long = "__test-run", global = true, hide = true)]
    pub test_run: bool,
    /// Allow for missing `packageManager` in `package.json`.
    ///
    /// `turbo` will use hints from codebase to guess which package manager
    /// should be used.
    #[clap(long, global = true)]
    pub dangerously_disable_package_manager_check: bool,
    #[clap(long = "experimental-allow-no-turbo-json", hide = true, global = true)]
    pub allow_no_turbo_json: bool,
    /// Use the `turbo.json` located at the provided path instead of one at the
    /// root of the repository.
    #[clap(long, global = true)]
    pub root_turbo_json: Option<Utf8PathBuf>,
    #[clap(flatten, next_help_heading = "Run Arguments")]
    // DO NOT MAKE THIS VISIBLE
    // This is explicitly set to None in `run`
    run_args: Option<RunArgs>,
    // This should be inside `RunArgs` but clap currently has a bug
    // around nested flattened optional args: https://github.com/clap-rs/clap/issues/4697
    #[clap(flatten)]
    // DO NOT MAKE THIS VISIBLE
    // Instead use the getter method execution_args()
    execution_args: Option<ExecutionArgs>,
    #[clap(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Parser, Clone, Copy, PartialEq, Eq, Default)]
pub struct Verbosity {
    #[clap(
        long = "verbosity",
        global = true,
        conflicts_with = "v",
        value_name = "COUNT"
    )]
    /// Verbosity level
    pub verbosity: Option<u8>,
    #[clap(
        short = 'v',
        action = clap::ArgAction::Count,
        global = true,
        hide = true,
        conflicts_with = "verbosity"
    )]
    pub v: u8,
}

impl From<Verbosity> for u8 {
    fn from(val: Verbosity) -> Self {
        let Verbosity { verbosity, v } = val;
        verbosity.unwrap_or(v)
    }
}

#[derive(Subcommand, Copy, Clone, Debug, PartialEq)]
pub enum DaemonCommand {
    /// Restarts the turbo daemon
    Restart,
    /// Ensures that the turbo daemon is running
    Start,
    /// Reports the status of the turbo daemon
    Status {
        /// Pass --json to report status in JSON format
        #[clap(long)]
        json: bool,
    },
    /// Stops the turbo daemon
    Stop,
    /// Stops the turbo daemon if it is already running, and removes any stale
    /// daemon state
    Clean {
        /// Clean
        #[clap(long, default_value_t = true)]
        clean_logs: bool,
    },
    /// Shows the daemon logs
    Logs,
}

#[derive(Copy, Clone, Debug, Default, ValueEnum, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Output in a human-readable format
    #[default]
    Pretty,
    /// Output in JSON format for direct parsing
    Json,
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OutputFormat::Pretty => "pretty",
            OutputFormat::Json => "json",
        })
    }
}

#[derive(Subcommand, Copy, Clone, Debug, PartialEq)]
pub enum TelemetryCommand {
    /// Enables anonymous telemetry
    Enable,
    /// Disables anonymous telemetry
    Disable,
    /// Reports the status of telemetry
    Status,
}

#[derive(Copy, Clone, Debug, PartialEq, ValueEnum)]
pub enum LinkTarget {
    RemoteCache,
    Spaces,
}

impl Args {
    pub fn new(os_args: Vec<OsString>) -> Self {
        let clap_args = match Args::parse(os_args) {
            Ok(args) => args,
            // Don't use error logger when displaying help text
            Err(e)
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) =>
            {
                let _ = e.print();
                process::exit(1);
            }
            Err(e) if e.use_stderr() => {
                let err_str = e.to_string();
                // A cleaner solution would be to implement our own clap::error::ErrorFormatter
                // but that would require copying the default formatter just to remove this
                // line: https://docs.rs/clap/latest/src/clap/error/format.rs.html#100
                error!(
                    "{}",
                    err_str.strip_prefix("error: ").unwrap_or(err_str.as_str())
                );
                process::exit(1);
            }
            // If the clap error shouldn't be printed to stderr it indicates help text
            Err(e) => {
                let _ = e.print();
                process::exit(0);
            }
        };
        // We have to override the --version flag because we use `get_version`
        // instead of a hard-coded version or the crate version
        if clap_args.version {
            println!("{}", get_version());
            process::exit(0);
        }

        if let Some(run_args) = clap_args.run_args() {
            if run_args.no_cache {
                warn!(
                    "--no-cache is deprecated and will be removed in a future major version. Use \
                     --cache=local:r,remote:r"
                );
            }
            if run_args.remote_only.is_some() {
                warn!(
                    "--remote-only is deprecated and will be removed in a future major version. \
                     Use --cache=remote:rw"
                );
            }
            if run_args.remote_cache_read_only.is_some() {
                warn!(
                    "--remote-cache-read-only is deprecated and will be removed in a future major \
                     version. Use --cache=local:rw,remote:r"
                );
            }
        }

        clap_args
    }

    fn parse(os_args: Vec<OsString>) -> Result<Self, clap::Error> {
        let (is_single_package, single_package_free) = Self::remove_single_package(os_args);
        let mut args = Args::try_parse_from(single_package_free)?;
        // And then only add them back in when we're in `run`.
        // The value can appear in two places in the struct.
        // We defensively attempt to set both.
        if let Some(ref mut execution_args) = args.execution_args {
            execution_args.single_package = is_single_package
        }

        if let Some(Command::Run {
            run_args: _,
            ref mut execution_args,
        }) = args.command
        {
            execution_args.single_package = is_single_package;
        }

        if env::var("TEST_RUN").is_ok() {
            args.test_run = true;
        }

        args.validate()?;

        Ok(args)
    }

    pub fn track(&self, tel: &GenericEventBuilder) {
        // track usage only
        track_usage!(tel, self.skip_infer, |val| val);
        track_usage!(tel, self.no_update_notifier, |val| val);
        track_usage!(tel, self.color, |val| val);
        track_usage!(tel, self.no_color, |val| val);
        track_usage!(tel, self.preflight, |val| val);
        track_usage!(tel, &self.login, Option::is_some);
        track_usage!(tel, &self.cwd, Option::is_some);
        track_usage!(tel, &self.heap, Option::is_some);
        track_usage!(tel, &self.team, Option::is_some);
        track_usage!(tel, &self.token, Option::is_some);
        track_usage!(tel, &self.trace, Option::is_some);
        track_usage!(tel, &self.api, Option::is_some);

        // track values
        if let Some(remote_cache_timeout) = self.remote_cache_timeout {
            tel.track_arg_value(
                "remote-cache-timeout",
                remote_cache_timeout,
                turborepo_telemetry::events::EventType::NonSensitive,
            );
        }
        if self.verbosity.v > 0 {
            tel.track_arg_value(
                "v",
                self.verbosity.v,
                turborepo_telemetry::events::EventType::NonSensitive,
            );
        }
        if let Some(verbosity) = self.verbosity.verbosity {
            tel.track_arg_value(
                "verbosity",
                verbosity,
                turborepo_telemetry::events::EventType::NonSensitive,
            );
        }
    }

    /// Fetch the run args supplied to the command
    pub fn run_args(&self) -> Option<&RunArgs> {
        if let Some(Command::Run { run_args, .. }) = &self.command {
            Some(run_args)
        } else {
            self.run_args.as_ref()
        }
    }

    /// Fetch the execution args supplied to the command
    pub fn execution_args(&self) -> Option<&ExecutionArgs> {
        if let Some(Command::Run { execution_args, .. }) = &self.command {
            Some(execution_args)
        } else {
            self.execution_args.as_ref()
        }
    }

    fn remove_single_package(args: Vec<OsString>) -> (bool, impl Iterator<Item = OsString>) {
        // We always pass --single-package in from the shim.
        // We need to omit it, and then add it in for run.
        let arg_separator_position = args.iter().position(|input_token| input_token == "--");

        let single_package_position = args
            .iter()
            .position(|input_token| input_token == "--single-package");

        let is_single_package = match (arg_separator_position, single_package_position) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(arg_separator_position), Some(single_package_position)) => {
                single_package_position < arg_separator_position
            }
        };

        // Clap supports arbitrary iterators as input.
        // We can remove all instances of --single-package
        let single_package_free = args
            .into_iter()
            .enumerate()
            .filter(move |(index, input_token)| {
                arg_separator_position
                    .is_some_and(|arg_separator_position| index > &arg_separator_position)
                    || input_token != "--single-package"
            })
            .map(|(_, input_token)| input_token);

        (is_single_package, single_package_free)
    }

    fn validate(&self) -> Result<(), clap::Error> {
        if self.run_args.is_some()
            && !matches!(
                self.command,
                None | Some(Command::Run { .. }) | Some(Command::Config)
            )
        {
            let mut cmd = Self::command();
            Err(cmd.error(
                clap::error::ErrorKind::UnknownArgument,
                "Cannot use run arguments outside of run command",
            ))
        } else if self.execution_args.is_some() && matches!(self.command, Some(Command::Watch(_))) {
            let mut cmd = Self::command();
            Err(cmd.error(
                clap::error::ErrorKind::ArgumentConflict,
                "Cannot use watch arguments before `watch` subcommand",
            ))
        } else if matches!(self.command, Some(Command::Run { .. }))
            && (self.run_args.is_some() || self.execution_args.is_some())
        {
            let mut cmd = Self::command();
            Err(cmd.error(
                clap::error::ErrorKind::ArgumentConflict,
                "Cannot use run arguments before `run` subcommand",
            ))
        } else {
            Ok(())
        }
    }
}

/// Defines the subcommands for CLI
#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum Command {
    /// Get the path to the Turbo binary
    Bin,
    /// Generate the autocompletion script for the specified shell
    Completion {
        shell: Shell,
    },
    /// Runs the Turborepo background daemon
    Daemon {
        /// Set the idle timeout for turbod
        #[clap(long, default_value_t = String::from("4h0m0s"))]
        idle_time: String,
        #[clap(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// Generate a new app / package
    #[clap(aliases = ["g", "gen"])]
    Generate {
        #[clap(long, default_value_t = String::from("latest"), hide = true)]
        tag: String,
        /// The name of the generator to run
        generator_name: Option<String>,
        /// Generator configuration file
        #[clap(short = 'c', long)]
        config: Option<String>,
        /// The root of your repository (default: directory with root
        /// turbo.json)
        #[clap(short = 'r', long)]
        root: Option<String>,
        /// Answers passed directly to generator
        #[clap(short = 'a', long, num_args = 1..)]
        args: Vec<String>,

        #[clap(subcommand)]
        command: Option<Box<GenerateCommand>>,
    },
    /// Enable or disable anonymous telemetry
    Telemetry {
        #[clap(subcommand)]
        command: Option<TelemetryCommand>,
    },
    /// Turbo your monorepo by running a number of 'repo lints' to
    /// identify common issues, suggest fixes, and improve performance.
    Scan,
    #[clap(hide = true)]
    Config,
    /// EXPERIMENTAL: List packages in your monorepo.
    Ls {
        /// Show only packages that are affected by changes between
        /// the current branch and `main`
        #[clap(long, group = "scope-filter-group")]
        affected: bool,
        /// Use the given selector to specify package(s) to act as
        /// entry points. The syntax mirrors pnpm's syntax, and
        /// additional documentation and examples can be found in
        /// turbo's documentation https://turbo.build/repo/docs/reference/command-line-reference/run#--filter
        #[clap(short = 'F', long, group = "scope-filter-group")]
        filter: Vec<String>,
        /// Get insight into a specific package, such as
        /// its dependencies and tasks
        packages: Vec<String>,
        /// Output format
        #[clap(long, value_enum)]
        output: Option<OutputFormat>,
    },
    /// Link your local directory to a Vercel organization and enable remote
    /// caching.
    Link {
        /// Do not create or modify .gitignore (default false)
        #[clap(long)]
        no_gitignore: bool,

        /// The scope, i.e. Vercel team, to which you are linking
        #[clap(long)]
        scope: Option<String>,

        /// Answer yes to all prompts (default false)
        #[clap(long, short)]
        yes: bool,
        /// Specify what should be linked (default "remote cache")
        #[clap(long, value_enum, default_value_t = LinkTarget::RemoteCache)]
        target: LinkTarget,
    },
    /// Login to your Vercel account
    Login {
        #[clap(long = "sso-team")]
        sso_team: Option<String>,
        /// Force a login to receive a new token. Will overwrite any existing
        /// tokens for the given login url.
        #[clap(long = "force", short = 'f')]
        force: bool,
    },
    /// Logout to your Vercel account
    Logout {
        /// Invalidate the token on the server
        #[clap(long)]
        invalidate: bool,
    },
    /// Print debugging information
    Info,
    /// Prepare a subset of your monorepo.
    Prune {
        #[clap(hide = true, long)]
        scope: Option<Vec<String>>,
        /// Workspaces that should be included in the subset
        #[clap(
            required_unless_present("scope"),
            conflicts_with("scope"),
            value_name = "SCOPE"
        )]
        scope_arg: Option<Vec<String>>,
        #[clap(long)]
        docker: bool,
        #[clap(long = "out-dir", default_value_t = String::from(prune::DEFAULT_OUTPUT_DIR), value_parser)]
        output_dir: String,
        /// Ignore files to be used during the prune process
        #[clap(long, value_parser)]
        ignore_files: Option<Vec<String>>,
    },

    /// Run tasks across projects in your monorepo
    ///
    /// By default, turbo executes tasks in topological order (i.e.
    /// dependencies first) and then caches the results. Re-running commands for
    /// tasks already in the cache will skip re-execution and immediately move
    /// artifacts from the cache into the correct output folders (as if the task
    /// occurred again).
    ///
    /// Arguments passed after '--' will be passed through to the named tasks.
    Run {
        #[clap(flatten)]
        run_args: Box<RunArgs>,
        #[clap(flatten)]
        execution_args: Box<ExecutionArgs>,
    },
    /// Query your monorepo using GraphQL. If no query is provided, spins up a
    /// GraphQL server with GraphiQL.
    Query {
        /// Pass variables to the query via a JSON file
        #[clap(short = 'V', long, requires = "query")]
        variables: Option<Utf8PathBuf>,
        /// The query to run, either a file path or a query string
        query: Option<String>,
    },
    Watch(Box<ExecutionArgs>),
    /// Unlink the current directory from your Vercel organization and disable
    /// Remote Caching
    Unlink {
        /// Specify what should be unlinked (default "remote cache")
        #[clap(long, value_enum, default_value_t = LinkTarget::RemoteCache)]
        target: LinkTarget,
    },
}

#[derive(Parser, Clone, Debug, Default, Serialize, PartialEq)]
pub struct GenerateWorkspaceArgs {
    /// Name for the new workspace
    #[clap(short = 'n', long)]
    pub name: Option<String>,
    /// Generate an empty workspace
    #[clap(short = 'b', long, conflicts_with = "copy", default_value_t = true)]
    pub empty: bool,
    /// Generate a workspace using an existing workspace as a template. Can be
    /// the name of a local workspace within your monorepo, or a fully
    /// qualified GitHub URL with any branch and/or subdirectory
    #[clap(short = 'c', long, conflicts_with = "empty", num_args = 0..=1, default_missing_value = "")]
    pub copy: Option<String>,
    /// Where the new workspace should be created
    #[clap(short = 'd', long)]
    pub destination: Option<String>,
    /// The type of workspace to create
    #[clap(short = 't', long)]
    pub r#type: Option<String>,
    /// The root of your repository (default: directory with root turbo.json)
    #[clap(short = 'r', long)]
    pub root: Option<String>,
    /// In a rare case, your GitHub URL might contain a branch name with a slash
    /// (e.g. bug/fix-1) and the path to the example (e.g. foo/bar). In this
    /// case, you must specify the path to the example separately:
    /// --example-path foo/bar
    #[clap(short = 'p', long)]
    pub example_path: Option<String>,
    /// Do not filter available dependencies by the workspace type
    #[clap(long, default_value_t = false)]
    pub show_all_dependencies: bool,
}

#[derive(Parser, Clone, Debug, Default, PartialEq, Serialize)]
pub struct GeneratorCustomArgs {
    /// The name of the generator to run
    generator_name: Option<String>,
    /// Generator configuration file
    #[clap(short = 'c', long)]
    config: Option<String>,
    /// The root of your repository (default: directory with root
    /// turbo.json)
    #[clap(short = 'r', long)]
    root: Option<String>,
    /// Answers passed directly to generator
    #[clap(short = 'a', long, value_delimiter = ' ', num_args = 1..)]
    args: Vec<String>,
}

#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum GenerateCommand {
    /// Add a new package or app to your project
    #[clap(name = "workspace", alias = "w")]
    Workspace(GenerateWorkspaceArgs),
    #[clap(name = "run", alias = "r")]
    Run(GeneratorCustomArgs),
}

fn validate_graph_extension(s: &str) -> Result<String, String> {
    match s.is_empty() {
        true => Ok(s.to_string()),
        _ => match Utf8Path::new(s).extension() {
            Some(ext) if SUPPORTED_GRAPH_FILE_EXTENSIONS.contains(&ext) => Ok(s.to_string()),
            Some(ext) => Err(format!(
                "Invalid file extension: '{}'. Allowed extensions are: {:?}",
                ext, SUPPORTED_GRAPH_FILE_EXTENSIONS
            )),
            None => Err(format!(
                "The provided filename is missing a file extension. Allowed extensions are: {:?}",
                SUPPORTED_GRAPH_FILE_EXTENSIONS
            )),
        },
    }
}

fn path_non_empty(s: &str) -> Result<Utf8PathBuf, String> {
    if s.is_empty() {
        Err("path must not be empty".to_string())
    } else {
        Ok(Utf8Path::new(s).to_path_buf())
    }
}

/// Arguments used in run and watch
#[derive(Parser, Clone, Debug, Default, PartialEq)]
#[command(groups = [
ArgGroup::new("scope-filter-group").multiple(true).required(false),
])]
pub struct ExecutionArgs {
    /// Override the filesystem cache directory.
    #[clap(long, value_parser = path_non_empty)]
    pub cache_dir: Option<Utf8PathBuf>,
    /// Limit the concurrency of task execution. Use 1 for serial (i.e.
    /// one-at-a-time) execution.
    #[clap(long)]
    pub concurrency: Option<String>,
    /// Continue execution even if a task exits with an error or non-zero
    /// exit code. The default behavior is to bail
    #[clap(long = "continue")]
    pub continue_execution: bool,
    /// Run turbo in single-package mode
    #[clap(long)]
    pub single_package: bool,
    /// Specify whether or not to do framework inference for tasks
    #[clap(long, value_name = "BOOL", action = ArgAction::Set, default_value = "true", default_missing_value = "true", num_args = 0..=1)]
    pub framework_inference: bool,
    /// Specify glob of global filesystem dependencies to be hashed. Useful
    /// for .env and files
    #[clap(long = "global-deps", action = ArgAction::Append)]
    pub global_deps: Vec<String>,
    /// Environment variable mode.
    /// Use "loose" to pass the entire existing environment.
    /// Use "strict" to use an allowlist specified in turbo.json.
    #[clap(long = "env-mode", num_args = 0..=1, default_missing_value = "strict")]
    pub env_mode: Option<EnvMode>,
    /// Use the given selector to specify package(s) to act as
    /// entry points. The syntax mirrors pnpm's syntax, and
    /// additional documentation and examples can be found in
    /// turbo's documentation https://turbo.build/repo/docs/reference/command-line-reference/run#--filter
    #[clap(short = 'F', long, group = "scope-filter-group")]
    pub filter: Vec<String>,

    /// Run only tasks that are affected by changes between
    /// the current branch and `main`
    #[clap(long, group = "scope-filter-group", conflicts_with = "filter")]
    pub affected: bool,

    /// Set type of process output logging. Use "full" to show
    /// all output. Use "hash-only" to show only turbo-computed
    /// task hashes. Use "new-only" to show only new output with
    /// only hashes for cached tasks. Use "none" to hide process
    /// output. (default full)
    #[clap(long, value_enum)]
    pub output_logs: Option<OutputLogsMode>,
    /// Set type of task output order. Use "stream" to show
    /// output as soon as it is available. Use "grouped" to
    /// show output when a command has finished execution. Use "auto" to let
    /// turbo decide based on its own heuristics. (default auto)
    #[clap(long, value_enum)]
    pub log_order: Option<LogOrder>,
    /// Only executes the tasks specified, does not execute parent tasks.
    #[clap(long)]
    pub only: bool,
    #[clap(long, hide = true)]
    pub pkg_inference_root: Option<String>,
    /// Use "none" to remove prefixes from task logs. Use "task" to get task id
    /// prefixing. Use "auto" to let turbo decide how to prefix the logs
    /// based on the execution environment. In most cases this will be the same
    /// as "task". Note that tasks running in parallel interleave their
    /// logs, so removing prefixes can make it difficult to associate logs
    /// with tasks. Use --log-order=grouped to prevent interleaving. (default
    /// auto)
    #[clap(long, value_enum, default_value_t = LogPrefix::Auto)]
    pub log_prefix: LogPrefix,
    // NOTE: The following two are hidden because clap displays them in the help text incorrectly:
    // > Usage: turbo [OPTIONS] [TASKS]... [-- <FORWARDED_ARGS>...] [COMMAND]
    #[clap(hide = true)]
    pub tasks: Vec<String>,
    #[clap(last = true, hide = true)]
    pub pass_through_args: Vec<String>,
}

impl ExecutionArgs {
    fn track(&self, telemetry: &CommandEventBuilder) {
        // default to false
        track_usage!(telemetry, self.framework_inference, |val: bool| !val);

        track_usage!(telemetry, self.continue_execution, |val| val);
        track_usage!(telemetry, self.single_package, |val| val);
        track_usage!(telemetry, self.only, |val| val);
        track_usage!(telemetry, &self.cache_dir, Option::is_some);
        track_usage!(telemetry, &self.pkg_inference_root, Option::is_some);

        if let Some(concurrency) = &self.concurrency {
            telemetry.track_arg_value("concurrency", concurrency, EventType::NonSensitive);
        }

        if !self.global_deps.is_empty() {
            telemetry.track_arg_value(
                "global-deps",
                self.global_deps.join(", "),
                EventType::NonSensitive,
            );
        }

        if let Some(env_mode) = self.env_mode {
            telemetry.track_arg_value("env-mode", env_mode, EventType::NonSensitive);
        }

        if let Some(output_logs) = &self.output_logs {
            telemetry.track_arg_value("output-logs", output_logs, EventType::NonSensitive);
        }

        if let Some(log_order) = self.log_order {
            telemetry.track_arg_value("log-order", log_order, EventType::NonSensitive);
        }

        if self.log_prefix != LogPrefix::default() {
            telemetry.track_arg_value("log-prefix", self.log_prefix, EventType::NonSensitive);
        }

        // track sizes
        if !self.filter.is_empty() {
            telemetry.track_arg_value("filter:length", self.filter.len(), EventType::NonSensitive);
        }
    }
}

#[derive(Parser, Clone, Debug, PartialEq)]
#[command(groups = [
    ArgGroup::new("daemon-group").multiple(false).required(false),
])]
pub struct RunArgs {
    /// Set the cache behavior for this run. Pass a list of comma-separated key,
    /// value pairs to enable reading and writing to either the local or
    /// remote cache.
    #[clap(long, conflicts_with_all = &["force", "remote_only", "remote_cache_read_only", "no_cache"])]
    pub cache: Option<String>,
    /// Ignore the existing cache (to force execution). Equivalent to
    /// `--cache=local:w,remote:w`
    #[clap(long, default_missing_value = "true")]
    pub force: Option<Option<bool>>,
    /// Ignore the local filesystem cache for all tasks. Only
    /// allow reading and caching artifacts using the remote cache.
    /// Equivalent to `--cache=remote:rw`
    #[clap(long, default_missing_value = "true", group = "cache-group")]
    pub remote_only: Option<Option<bool>>,
    /// Treat remote cache as read only. Equivalent to
    /// `--cache=remote:r;local:rw`
    #[clap(long, default_missing_value = "true")]
    pub remote_cache_read_only: Option<Option<bool>>,
    /// Avoid saving task results to the cache. Useful for development/watch
    /// tasks. Equivalent to `--cache=local:r,remote:r`
    #[clap(long)]
    pub no_cache: bool,

    /// Set the number of concurrent cache operations (default 10)
    #[clap(long, default_value_t = DEFAULT_NUM_WORKERS)]
    pub cache_workers: u32,
    #[clap(alias = "dry", long = "dry-run", num_args = 0..=1, default_missing_value = "text")]
    pub dry_run: Option<DryRunMode>,
    /// Generate a graph of the task execution and output to a file when a
    /// filename is specified (.svg, .png, .jpg, .pdf, .json,
    /// .html, .mermaid, .dot). Outputs dot graph to stdout when if no filename
    /// is provided
    #[clap(long, num_args = 0..=1, default_missing_value = "", value_parser = validate_graph_extension)]
    pub graph: Option<String>,
    // clap does not have negation flags such as --daemon and --no-daemon
    // so we need to use a group to enforce that only one of them is set.
    // -----------------------
    /// Force turbo to use the local daemon. If unset
    /// turbo will use the default detection logic.
    #[clap(long, group = "daemon-group")]
    pub daemon: bool,

    /// Force turbo to not use the local daemon. If unset
    /// turbo will use the default detection logic.
    #[clap(long, group = "daemon-group")]
    pub no_daemon: bool,

    /// File to write turbo's performance profile output into.
    /// You can load the file up in chrome://tracing to see
    /// which parts of your build were slow.
    #[clap(long, value_parser=NonEmptyStringValueParser::new(), conflicts_with = "anon_profile")]
    pub profile: Option<String>,
    /// File to write turbo's performance profile output into.
    /// All identifying data omitted from the profile.
    #[clap(long, value_parser=NonEmptyStringValueParser::new(), conflicts_with = "profile")]
    pub anon_profile: Option<String>,
    /// Generate a summary of the turbo run
    #[clap(long, default_missing_value = "true")]
    pub summarize: Option<Option<bool>>,

    // Pass a string to enable posting Run Summaries to Vercel
    #[clap(long, hide = true)]
    pub experimental_space_id: Option<String>,

    /// Execute all tasks in parallel.
    #[clap(long)]
    pub parallel: bool,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            remote_only: None,
            cache: None,
            force: None,
            cache_workers: DEFAULT_NUM_WORKERS,
            dry_run: None,
            graph: None,
            no_cache: false,
            daemon: false,
            no_daemon: false,
            profile: None,
            anon_profile: None,
            remote_cache_read_only: None,
            summarize: None,
            experimental_space_id: None,
            parallel: false,
        }
    }
}

impl RunArgs {
    pub fn remote_only(&self) -> Option<bool> {
        let remote_only = self.remote_only?;
        Some(remote_only.unwrap_or(true))
    }

    /// Some(true) means force the daemon
    /// Some(false) means force no daemon
    /// None means use the default detection
    pub fn daemon(&self) -> Option<bool> {
        match (self.daemon, self.no_daemon) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
            (true, true) => unreachable!(), // guaranteed by mutually exclusive `ArgGroup`
        }
    }

    pub fn profile_file_and_include_args(&self) -> Option<(&str, bool)> {
        match (self.profile.as_deref(), self.anon_profile.as_deref()) {
            (Some(file), None) => Some((file, true)),
            (None, Some(file)) => Some((file, false)),
            (Some(_), Some(_)) => unreachable!(),
            (None, None) => None,
        }
    }

    pub fn remote_cache_read_only(&self) -> Option<bool> {
        let remote_cache_read_only = self.remote_cache_read_only?;
        Some(remote_cache_read_only.unwrap_or(true))
    }

    pub fn summarize(&self) -> Option<bool> {
        let summarize = self.summarize?;
        Some(summarize.unwrap_or(true))
    }

    pub fn track(&self, telemetry: &CommandEventBuilder) {
        // default to true
        track_usage!(telemetry, self.no_cache, |val| val);
        track_usage!(telemetry, self.remote_only().unwrap_or_default(), |val| val);
        track_usage!(telemetry, &self.force, Option::is_some);
        track_usage!(telemetry, self.daemon, |val| val);
        track_usage
