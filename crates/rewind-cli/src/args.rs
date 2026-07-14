use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "rewind",
    version,
    about = "Run any coding agent inside a branchable, replayable workspace",
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub(crate) verbose: u8,

    #[arg(long, global = true, value_enum, default_value_t = LogFormatArg::Text)]
    pub(crate) log_format: LogFormatArg,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Initialize or validate the local Rewind store.
    Init,
    /// Record a command inside an isolated workspace.
    #[command(trailing_var_arg = true)]
    Run {
        /// Source workspace; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Override terminal-input recording policy.
        #[arg(long, value_enum)]
        record_input: Option<RecordInputArg>,
        /// Print only the completed run ID after child output.
        #[arg(long)]
        id_only: bool,
        /// Command and arguments following `--`.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// List recorded runs.
    List {
        /// Emit stable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one run, its checkpoints, and typed events.
    Show {
        /// Full run ID or an unambiguous prefix.
        run: String,
        /// Select a checkpoint ID, label, `initial`, or `final`.
        #[arg(long)]
        checkpoint: Option<String>,
        /// Print only the selected checkpoint ID.
        #[arg(long, requires = "checkpoint")]
        id_only: bool,
        /// Emit stable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Play back a recorded run; defaults to the latest run.
    Replay {
        /// Full run ID or an unambiguous prefix.
        run: Option<String>,
    },
    /// Create a manual checkpoint in the active recording.
    Mark {
        /// Human-readable checkpoint label.
        label: Option<String>,
    },
    /// Materialize a checkpoint into an explicit destination.
    Checkout {
        /// `<run>@<checkpoint>` where checkpoint is an ID, label, `initial`, or `final`.
        selector: String,
        /// Destination directory.
        #[arg(long)]
        to: PathBuf,
        /// Replace an existing nonempty destination.
        #[arg(long)]
        force: bool,
    },
    /// Start a child run from a recorded checkpoint.
    #[command(trailing_var_arg = true)]
    Fork {
        /// `<run>@<checkpoint>` where checkpoint is an ID, label, `initial`, or `final`.
        selector: String,
        /// Override terminal-input recording policy.
        #[arg(long, value_enum)]
        record_input: Option<RecordInputArg>,
        /// Print only the completed child run ID after command output.
        #[arg(long)]
        id_only: bool,
        /// Command and arguments following `--`.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        command: Vec<OsString>,
    },
    /// Compare two recorded runs using filesystem and execution evidence.
    Compare {
        /// Left run ID or unambiguous prefix.
        run_a: String,
        /// Right run ID or unambiguous prefix.
        run_b: String,
        /// Shell command evaluated independently in both final snapshots.
        #[arg(long)]
        test: Option<String>,
        /// Emit stable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Export a replay bundle or self-contained HTML artifact.
    Export {
        /// Run ID or unambiguous prefix.
        run: String,
        /// Export artifact format.
        #[arg(long, value_enum)]
        format: ExportFormatArg,
        /// Output file; a deterministic name is used when omitted.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Inspect local capabilities, storage, recovery, and corruption signals.
    Doctor {
        /// Emit stable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Report unreachable content; deletion is always explicit.
    Gc {
        /// Delete unreachable objects after reporting them.
        #[arg(long)]
        delete: bool,
        /// Emit stable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Generate shell completion files from the live CLI definition.
    Completions {
        /// Shell to generate, or `all`.
        #[arg(long, value_enum)]
        shell: CompletionShellArg,
        /// Output directory.
        #[arg(long)]
        output: PathBuf,
    },
    /// Generate the local manual page from the live CLI definition.
    Man {
        /// Output roff file.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum RecordInputArg {
    Auto,
    Always,
    Never,
}

impl From<RecordInputArg> for rewind_domain::InputRecordingPolicy {
    fn from(value: RecordInputArg) -> Self {
        match value {
            RecordInputArg::Auto => Self::Auto,
            RecordInputArg::Always => Self::Always,
            RecordInputArg::Never => Self::Never,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum ExportFormatArg {
    Bundle,
    Html,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CompletionShellArg {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum LogFormatArg {
    Text,
    Json,
}
