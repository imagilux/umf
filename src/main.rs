//! `umf` CLI — entrypoint for the Universal Machine Format reference
//! implementation.
//!
//! This file is a thin shim: it declares the two top-level modules
//! ([`mod@cli`] — argument parsing + subcommand dispatch, and
//! [`mod@render`] — the `umf parse` table renderer) and hands control
//! to `cli::run`. Every subcommand's implementation lives under
//! `src/cli/`.

use std::process::ExitCode;

mod cli;
mod render;

fn main() -> ExitCode {
    cli::run()
}
