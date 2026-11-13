mod agent;
mod awake;
mod cli;
mod collector;
mod config;
mod daemon;
mod diagnostics;
mod launch;
mod log;
mod preferences;
mod project;
mod prompts;
mod protected;
mod protected_write;
mod replay;
mod rolling;
mod sandbox;
mod service;
mod session;
mod settings;
mod setup;
mod state;
mod tailnet;
mod view;
mod viewconn;
mod watch;
mod websocket;
mod wire;
mod worker;
mod workspaces;

use std::process::ExitCode;

/// `<package version> (<git commit>[-dirty])`, from build.rs.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PITON_BUILD"), ")");

fn main() -> ExitCode {
    cli::main()
}
