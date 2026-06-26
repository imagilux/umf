//! `umf ps` — list umf-managed processes (builds + runs) from the
//! [process registry](super::process), with filtering, sorting, and
//! pretty / plain / json output.

use std::process::ExitCode;

use clap::ValueEnum;

use super::process::{ProcessRecord, ProcessRegistry, ProcessStatus, now_epoch};

/// Output format for `umf ps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum PsOutput {
    /// Column-aligned table (default).
    Pretty,
    /// Tab-separated, header + rows — scriptable.
    Plain,
    /// JSON array of the raw process records.
    Json,
}

/// Columns, in display order. `(header, sort/filter key)`.
const COLUMNS: &[(&str, &str)] = &[
    ("ID", "id"),
    ("NAME", "name"),
    ("PROCESS", "process"),
    ("TYPE", "type"),
    ("STATUS", "status"),
    ("RELEASE", "release"),
    ("STARTED", "started"),
];

/// Valid filter / sort keys (`type` is an alias for the process kind).
const KEYS: &[&str] = &[
    "id", "name", "process", "type", "status", "release", "started",
];

/// `umf ps` entry point.
pub(crate) fn run_ps(
    output: PsOutput,
    sort: Option<&str>,
    filters: &[String],
    prune: bool,
) -> ExitCode {
    let registry = match ProcessRegistry::open() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot open process registry: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut records = match registry.list() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot read process registry: {e}");
            return ExitCode::FAILURE;
        }
    };

    let criteria = match parse_filters(filters) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    records.retain(|r| criteria.iter().all(|(k, v)| matches(r, k, v)));

    let (key, descending) = match parse_sort(sort) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    sort_records(&mut records, &key, descending);

    if prune {
        return do_prune(&registry, records, output);
    }

    match output {
        PsOutput::Pretty => print_table(&records, true),
        PsOutput::Plain => print_table(&records, false),
        PsOutput::Json => match serde_json::to_string_pretty(&records) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("error: serialising records: {e}");
                return ExitCode::FAILURE;
            }
        },
    }
    ExitCode::SUCCESS
}

/// Delete the finished (exited/failed) records among `records` — a running
/// process is never pruned. `records` is already `--filter`-ed and
/// `--sort`-ed, so prune honours both. Output mirrors `--output`: a count
/// (pretty), the pruned ids (plain), or the pruned records (json).
fn do_prune(registry: &ProcessRegistry, records: Vec<ProcessRecord>, output: PsOutput) -> ExitCode {
    let mut pruned = Vec::new();
    let mut kept_running = 0usize;
    let mut errors = 0usize;
    for record in records {
        if record.status == ProcessStatus::Running {
            kept_running += 1;
            continue;
        }
        match registry.remove(&record.id) {
            Ok(()) => pruned.push(record),
            Err(e) => {
                eprintln!("warning: could not remove {}: {e}", record.id);
                errors += 1;
            }
        }
    }
    match output {
        PsOutput::Json => match serde_json::to_string_pretty(&pruned) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("error: serialising pruned records: {e}");
                return ExitCode::FAILURE;
            }
        },
        PsOutput::Plain => {
            for r in &pruned {
                println!("{}", r.id);
            }
        }
        PsOutput::Pretty => {
            println!("Pruned {} finished process record(s).", pruned.len());
            if kept_running > 0 {
                println!("Kept {kept_running} still running.");
            }
        }
    }
    if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ── Filtering ─────────────────────────────────────────────────────────────

/// Parse `--filter` values: each is a comma-separated list of `KEY=VALUE`
/// (or `KEY:VALUE`); all criteria across all flags are ANDed. `VALUE` of
/// `all` or `*` matches anything.
fn parse_filters(filters: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for spec in filters {
        for clause in spec.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let (key, value) = clause
                .split_once('=')
                .or_else(|| clause.split_once(':'))
                .ok_or_else(|| {
                    format!("invalid filter `{clause}` — expected KEY=VALUE (e.g. STATUS=exited)")
                })?;
            let key = key.trim().to_ascii_lowercase();
            if !KEYS.contains(&key.as_str()) {
                return Err(format!(
                    "unknown filter key `{key}` — valid keys: {}",
                    KEYS.join(", ")
                ));
            }
            out.push((key, value.trim().to_string()));
        }
    }
    Ok(out)
}

/// Case-insensitive substring match; `all` / `*` is a wildcard.
fn matches(record: &ProcessRecord, key: &str, value: &str) -> bool {
    if value.eq_ignore_ascii_case("all") || value == "*" {
        return true;
    }
    field(record, key)
        .to_ascii_lowercase()
        .contains(&value.to_ascii_lowercase())
}

// ── Sorting ───────────────────────────────────────────────────────────────

/// Parse `--sort`: `KEY`, `KEY:asc`, `KEY:desc`, or a bare `asc` / `desc`
/// (which sorts the default `started` column). Defaults to newest-first
/// (`started` descending).
fn parse_sort(sort: Option<&str>) -> Result<(String, bool), String> {
    let Some(raw) = sort.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(("started".to_string(), true));
    };
    let lower = raw.to_ascii_lowercase();
    // Bare direction → default column.
    if lower == "asc" {
        return Ok(("started".to_string(), false));
    }
    if lower == "desc" {
        return Ok(("started".to_string(), true));
    }
    let (key, dir) = match lower.split_once(':') {
        Some((k, d)) => (k.to_string(), Some(d.to_string())),
        None => (lower, None),
    };
    if !KEYS.contains(&key.as_str()) {
        return Err(format!(
            "unknown sort key `{key}` — valid keys: {} (optionally `:asc`/`:desc`)",
            KEYS.join(", ")
        ));
    }
    let descending = match dir.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(other) => {
            return Err(format!(
                "unknown sort direction `{other}` — use asc or desc"
            ));
        }
    };
    Ok((key, descending))
}

fn sort_records(records: &mut [ProcessRecord], key: &str, descending: bool) {
    records.sort_by(|a, b| {
        let ord = if key == "started" {
            a.started_epoch.cmp(&b.started_epoch)
        } else {
            field(a, key)
                .to_ascii_lowercase()
                .cmp(&field(b, key).to_ascii_lowercase())
        };
        if descending { ord.reverse() } else { ord }
    });
}

// ── Field access + rendering ───────────────────────────────────────────────

/// String value of a record's column (for filter/sort). `started` returns
/// the raw epoch here; display formatting happens in [`cell`].
fn field(r: &ProcessRecord, key: &str) -> String {
    match key {
        "id" => r.id.clone(),
        "name" => r.name.clone(),
        "process" => r.process.clone(),
        "type" => r.kind.as_str().to_string(),
        "status" => r.status.as_str().to_string(),
        "release" => r.release.clone().unwrap_or_default(),
        "started" => r.started_epoch.to_string(),
        _ => String::new(),
    }
}

/// Display value of a record's column (human-friendly).
fn cell(r: &ProcessRecord, key: &str) -> String {
    match key {
        "status" => match (r.status.as_str(), r.exit_code) {
            ("exited", Some(code)) => format!("exited ({code})"),
            (s, _) => s.to_string(),
        },
        "release" => r.release.clone().unwrap_or_else(|| "-".to_string()),
        "started" => age(r.started_epoch),
        other => {
            let v = field(r, other);
            if v.is_empty() { "-".to_string() } else { v }
        }
    }
}

/// Render the records as a table: `pretty` = column-aligned; otherwise
/// tab-separated (scriptable). Both print the header row.
fn print_table(records: &[ProcessRecord], pretty: bool) {
    let headers: Vec<&str> = COLUMNS.iter().map(|(h, _)| *h).collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|r| COLUMNS.iter().map(|(_, k)| cell(r, k)).collect())
        .collect();

    if !pretty {
        println!("{}", headers.join("\t"));
        for row in &rows {
            println!("{}", row.join("\t"));
        }
        return;
    }

    // Column widths: max of header + cell widths, capped so a long
    // reference/command can't blow out the table.
    const CAP: usize = 32;
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(CAP));
        }
    }
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let c = truncate(c, CAP);
                let pad = widths[i].saturating_sub(c.chars().count());
                format!("{c}{}", " ".repeat(pad))
            })
            .collect::<Vec<_>>()
            .join("   ")
            .trim_end()
            .to_string()
    };
    println!(
        "{}",
        line(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>())
    );
    for row in &rows {
        println!("{}", line(row));
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    }
}

/// Relative age of an epoch-seconds timestamp (`5s`, `3m`, `2h`, `4d` ago).
fn age(epoch: u64) -> String {
    let secs = now_epoch().saturating_sub(epoch);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests;
