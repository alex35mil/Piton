mod agent;
mod apps;
mod commands;
mod config;
mod daemon;
mod gate;
mod launch;
mod logging;
mod pi_catalog;
mod project;
mod session;
mod setup;
mod state;
mod tools;

use std::process::ExitCode;

/// `<package version> (<git commit>[-dirty])`, from build.rs.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PITON_BUILD"), ")");

fn main() -> ExitCode {
    // Invoked as `ssh` (the link a filtered network's sandbox finds first on PATH): the real ssh, through the gate.
    if gate::ssh::invoked_as_ssh() {
        eprintln!("piton: can't run ssh: {}", gate::ssh::exec_ssh());
        return ExitCode::from(255);
    }
    commands::main()
}
