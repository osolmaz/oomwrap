//! oomwrap runs one command and stops it before memory pressure freezes the
//! machine. The command line is the contract; this library has no stable API.

mod backend;
mod cli;
mod config;
mod doctor;
mod events;
mod host;
mod limits;
mod policy;
mod readings;
mod shims;
mod signal;
mod skills;
mod supervise;

pub use cli::Cli;

/// Runs oomwrap with the process arguments and returns the exit code.
pub fn main_entry() -> i32 {
    if let Some(code) = skills::handle_if_requested() {
        return code;
    }
    cli::run(&host::Host::from_env())
}
