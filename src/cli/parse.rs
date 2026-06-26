//! `umf parse` — exercise the full lexer + grammar + validation
//! pipeline and render the AST as a table, JSON, or Rust `Debug`.

use std::path::Path;
use std::process::ExitCode;

use crate::cli::ParseFormat;
use crate::cli::util;

/// Resolve the recipe (`path` positional + optional `-f/--file`), parse
/// it, and print the AST in the requested `format`. Renders ariadne
/// diagnostics to stderr and returns `FAILURE` on resolution or parse
/// error.
pub(crate) fn run_parse(path: Option<&Path>, file: Option<&Path>, format: ParseFormat) -> ExitCode {
    let resolved = match util::resolve_recipe(path, file) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let recipe = resolved.recipe.as_path();
    let source = match std::fs::read_to_string(recipe) {
        Ok(s) => s,
        Err(io_err) => {
            eprintln!("error: cannot read {}: {io_err}", recipe.display());
            return ExitCode::FAILURE;
        }
    };
    let source_name = recipe.display().to_string();
    match umf_parser::parse_with_warnings(&source) {
        Ok((ast, warnings)) => {
            if !warnings.is_empty() {
                let mut stderr = std::io::stderr().lock();
                let _ = umf_parser::diagnostics::report_all(
                    &warnings,
                    &mut stderr,
                    &source_name,
                    &source,
                );
            }
            match format {
                ParseFormat::Table => {
                    print!("{}", crate::render::render_ast(&ast, recipe));
                    ExitCode::SUCCESS
                }
                ParseFormat::Json => match serde_json::to_string_pretty(&ast) {
                    Ok(json) => {
                        println!("{json}");
                        ExitCode::SUCCESS
                    }
                    Err(json_err) => {
                        eprintln!("error: cannot serialize AST as JSON: {json_err}");
                        ExitCode::FAILURE
                    }
                },
                ParseFormat::Debug => {
                    println!("{ast:#?}");
                    ExitCode::SUCCESS
                }
            }
        }
        Err(diagnostics) => {
            let mut stderr = std::io::stderr().lock();
            let _ = umf_parser::diagnostics::report_all(
                &diagnostics,
                &mut stderr,
                &source_name,
                &source,
            );
            ExitCode::FAILURE
        }
    }
}
