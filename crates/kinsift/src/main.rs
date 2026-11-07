//! kinsift: group files by metadata, review the groups, act on them.

mod cli;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = cli::Args::parse();
    match cli::run(args) {
        Ok(()) => ExitCode::SUCCESS,
        // `kinsift query | head` closes the pipe early; that is not an error.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe))
}
