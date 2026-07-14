mod app;
mod args;
mod artifacts;
mod compare;
mod config;
mod doctor;
mod export;
mod gc;
mod output;
mod replay;
mod resolve;

use std::process::ExitCode;

use args::{Cli, LogFormatArg};
use clap::Parser;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = Cli::parse();
    initialize_tracing(cli.verbose, cli.log_format);
    match app::execute(cli.command) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("rewind: {error}");
            ExitCode::FAILURE
        }
    }
}

fn initialize_tracing(verbosity: u8, format: LogFormatArg) {
    let default = match verbosity {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    let filter = EnvFilter::builder()
        .with_default_directive(default.into())
        .from_env_lossy();
    let ansi = std::env::var_os("NO_COLOR").is_none();
    match format {
        LogFormatArg::Text => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(ansi)
                .with_writer(std::io::stderr)
                .try_init();
        }
        LogFormatArg::Json => {
            let _ = tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}
