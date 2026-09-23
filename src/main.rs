//! `flighty-wall`: sync Flighty Friends' flights to a `FlightWall` Mini.

use clap::Parser;
use flighty_wall::cli::{Cli, run};
use std::process::ExitCode;

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}
