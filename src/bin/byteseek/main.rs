//! `byteseek`: literal, fuzzy, boolean, and structured search with ripgrep's command line.

#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(byteseek::cli::run(env::args_os()))
}
