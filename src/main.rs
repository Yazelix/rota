//! rota picks which coding-agent subscription runs your next agent session.
//!
//! This is a stub: rota is in planning and does nothing yet.

use std::process::ExitCode;

const USAGE: &str = "\
rota: picks which coding-agent subscription runs your next agent session

Usage:
  rota launch [--role implement|review] [--level medium|high] [--only BACKEND] <prompt-file>
  rota status [--json]

rota is in planning and does nothing yet.";

fn main() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
