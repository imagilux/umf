//! Pretty-print an [`Ast`] as a table-style summary for the CLI.
//!
//! Counterpart to `--format=json` and `--format=debug`: same AST, friendlier
//! rendering for terminal use. Lossy by design — long RUN commands are
//! truncated, span offsets are dropped. When AST round-tripping matters,
//! use JSON.

use std::path::Path;

use umf_core::ast::{
    Ast, CmdForm, Directive, EntrypointInit, ExposeProtocol, FromSource, RunCommand, Stage,
};

/// Column width reserved for the directive name. Wider than the longest
/// keyword (`ENTRYPOINT` = 10) by enough padding to keep it aligned with the
/// rest.
const DIRECTIVE_COL_WIDTH: usize = 11;

/// Maximum width (in characters) for the rendered directive *value*. Long
/// values are truncated with an ellipsis so the table stays readable on
/// 80-column terminals.
const MAX_VALUE_WIDTH: usize = 80;

/// Total width (in characters) of the stage heading rule.
const HEADING_WIDTH: usize = 64;

/// Render `ast` (parsed from `source_path`) as a table-style summary.
#[must_use]
pub fn render_ast(ast: &Ast, source_path: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("File: {}\n", source_path.display()));
    let count = ast.stages.len();
    out.push_str(&format!(
        "{count} {}\n",
        if count == 1 { "stage" } else { "stages" }
    ));

    for (i, stage) in ast.stages.iter().enumerate() {
        out.push('\n');
        render_stage(stage, i + 1, &mut out);
    }
    out
}

fn render_stage(stage: &Stage, index: usize, out: &mut String) {
    // Header rule, e.g. `═══ Stage 1 ═════════════════════════════════`.
    let label = match &stage.name {
        Some(name) => format!("Stage {index} (AS {})", name.value),
        None => format!("Stage {index}"),
    };
    let lead = "═══ ";
    let trail_len = HEADING_WIDTH.saturating_sub(lead.chars().count() + label.chars().count() + 1);
    out.push_str(&format!("{lead}{label} {}\n", "═".repeat(trail_len.max(3))));

    // FROM is the structural first row.
    let from_value = match &stage.from.source {
        FromSource::Scratch => "scratch".to_string(),
        FromSource::Reference(r) => r.value.as_str().to_string(),
    };
    push_row(out, "FROM", &from_value);

    for directive in &stage.directives {
        render_directive(directive, out);
    }
}

fn render_directive(directive: &Directive, out: &mut String) {
    match directive {
        Directive::Label(d) => push_row(
            out,
            "LABEL",
            &format!("{} = {}", d.key.value, d.value.value),
        ),
        Directive::Env(d) => push_row(out, "ENV", &format!("{} = {}", d.key.value, d.value.value)),
        Directive::Arg(d) => {
            let value = match &d.default {
                Some(default) => format!("{} = {}", d.name.value, default.value),
                None => d.name.value.to_string(),
            };
            push_row(out, "ARG", &value);
        }
        Directive::Shell(d) => {
            let v = if d.argv.is_empty() {
                "none".to_string()
            } else {
                format_exec_form(&d.argv)
            };
            push_row(out, "SHELL", &v);
        }
        Directive::User(d) => push_row(out, "USER", d.name.value.as_str()),
        Directive::Workdir(d) => push_row(out, "WORKDIR", d.path.value.as_str()),
        Directive::Run(d) => {
            let cmd = match &d.command {
                RunCommand::Shell(s) => s.value.clone(),
                RunCommand::Exec(argv) => format_exec_form(argv),
            };
            push_row(out, "RUN", &cmd);
        }
        Directive::Add(d) => {
            let v = match &d.from {
                Some(s) => format!(
                    "{} → {}  (--from={})",
                    d.source.as_str(),
                    d.destination.value,
                    s.value
                ),
                None => format!("{} → {}", d.source.as_str(), d.destination.value),
            };
            push_row(out, if d.plain_copy { "COPY" } else { "ADD" }, &v);
        }
        Directive::Entrypoint(d) => {
            let v = match &d.init {
                EntrypointInit::Systemd => "systemd".to_string(),
                EntrypointInit::OpenRc => "openrc".to_string(),
                EntrypointInit::None => "none".to_string(),
                EntrypointInit::Path(s) => s.value.clone(),
                EntrypointInit::Exec(argv) => format_exec_form(argv),
            };
            push_row(out, "ENTRYPOINT", &v);
        }
        Directive::Expose(d) => {
            let proto = match d.protocol {
                ExposeProtocol::Tcp => "tcp",
                ExposeProtocol::Udp => "udp",
            };
            push_row(out, "EXPOSE", &format!("{}/{}", d.port, proto));
        }
        Directive::Cmd(d) => {
            let v = match &d.command {
                CmdForm::Shell(s) => s.value.clone(),
                CmdForm::Exec(argv) => format_exec_form(argv),
            };
            push_row(out, "CMD", &v);
        }
        Directive::Volume(d) => {
            let v = d
                .paths
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            push_row(out, "VOLUME", &v);
        }
        Directive::Stopsignal(d) => push_row(out, "STOPSIGNAL", d.signal.value.as_str()),
    }
}

fn format_exec_form(argv: &[umf_core::ast::Spanned<String>]) -> String {
    let parts: Vec<String> = argv
        .iter()
        .map(|a| format!("\"{}\"", a.value.replace('"', "\\\"")))
        .collect();
    format!("[{}]", parts.join(", "))
}

fn push_row(out: &mut String, name: &str, value: &str) {
    let value = truncate(value, MAX_VALUE_WIDTH);
    out.push_str(&format!("  {name:<DIRECTIVE_COL_WIDTH$} {value}\n"));
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut taken: String = s.chars().take(max.saturating_sub(1)).collect();
        taken.push('…');
        taken
    }
}

#[cfg(test)]
mod tests;
