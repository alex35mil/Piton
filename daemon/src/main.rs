mod agent;
mod apps;
mod cli;
mod config;
mod daemon;
mod launch;
mod logging;
mod project;
mod protected;
mod session;
mod setup;
mod state;

use std::process::ExitCode;

/// `<package version> (<git commit>[-dirty])`, from build.rs.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PITON_BUILD"), ")");

fn main() -> ExitCode {
    cli::main()
}
