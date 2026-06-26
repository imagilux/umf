//! `umf bench` — run a build N times (1 cold + N warm by default) and
//! report median / p99 / min / max wall-clock timing plus cache-
//! determinism flags. Drives the umf binary as a subprocess so each
//! iteration is a clean process with its own layout.

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;

use crate::cli::BenchFormat;
use crate::cli::util;

#[derive(Debug, Error)]
pub(crate) enum CliBenchError {
    #[error(transparent)]
    Recipe(#[from] util::RecipeResolveError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not locate the umf binary to drive the bench: {0}")]
    LocateBinary(std::io::Error),
    #[error("`umf build` failed during run {run}: {message}")]
    BuildFailed { run: String, message: String },
    #[error("could not parse metrics JSON from run {run}: {err}")]
    ParseMetrics { run: String, err: serde_json::Error },
    #[error("serialise bench report: {0}")]
    SerialiseReport(#[from] serde_json::Error),
}

/// Bundled `umf bench` flags.
pub(crate) struct BenchArgs<'a> {
    pub(crate) path: Option<&'a Path>,
    pub(crate) file: Option<&'a Path>,
    pub(crate) runs: usize,
    pub(crate) warmup: usize,
    pub(crate) cold_only: bool,
    pub(crate) format: BenchFormat,
    pub(crate) layout_dir_override: Option<&'a Path>,
    pub(crate) tag: &'a str,
}

/// Run the cold + warmup + measurement iterations and emit the
/// aggregated [`umf_builder::bench::BenchReport`].
pub(crate) fn run_bench(args: BenchArgs<'_>) -> Result<(), CliBenchError> {
    let umf_bin = std::env::current_exe().map_err(CliBenchError::LocateBinary)?;

    // Resolve the recipe once (discovery / -f), then drive every
    // subprocess `umf build` with the concrete file path.
    let resolved = util::resolve_recipe(args.path, args.file)?;
    let recipe = resolved.recipe.as_path();

    // Per-bench working dir. The `_workspace_guard` keeps the default
    // tempdir alive for the whole run so it is removed on drop.
    let (workspace, _workspace_guard) = bench_workspace(args.layout_dir_override)?;
    let layout_dir = workspace.join("layout");
    std::fs::create_dir_all(&layout_dir)?;
    let metrics_dir = workspace.join("metrics");
    std::fs::create_dir_all(&metrics_dir)?;

    eprintln!(
        "bench: working dir {} ({} warmup, {} measured run(s){})",
        workspace.display(),
        args.warmup,
        args.runs,
        if args.cold_only { ", cold-only" } else { "" },
    );

    // Cold run — clean cache before the first invocation.
    purge_layout(&layout_dir)?;
    let cold = run_one_bench_iteration(
        &umf_bin,
        recipe,
        &layout_dir,
        &metrics_dir,
        args.tag,
        "cold",
    )?;

    // Warmup runs are dropped (their metrics still get saved for
    // debugging but they don't enter the aggregate).
    for i in 0..args.warmup {
        let label = format!("warmup-{}", i + 1);
        let _ = run_one_bench_iteration(
            &umf_bin,
            recipe,
            &layout_dir,
            &metrics_dir,
            args.tag,
            &label,
        )?;
    }

    // Measurement runs.
    let mut warm_runs = Vec::with_capacity(if args.cold_only { 0 } else { args.runs });
    if !args.cold_only {
        for i in 0..args.runs {
            let label = format!("warm-{}", i + 1);
            let m = run_one_bench_iteration(
                &umf_bin,
                recipe,
                &layout_dir,
                &metrics_dir,
                args.tag,
                &label,
            )?;
            warm_runs.push(m);
        }
    }

    let report = umf_builder::bench::BenchReport::aggregate(
        recipe.display().to_string(),
        args.warmup,
        Some(cold),
        warm_runs,
    );

    match args.format {
        BenchFormat::Text => eprint!("{}", report.render_text()),
        BenchFormat::Json => {
            let json = serde_json::to_string_pretty(&report)?;
            println!("{json}");
        }
    }
    Ok(())
}

/// Pick the per-bench working directory.
///
/// With an explicit `--layout-dir` the caller owns the directory: it is
/// used as-is and stays persistent (no guard returned). Otherwise a
/// fresh tempdir keeps production caches pristine, and its [`TempDir`]
/// guard is returned so the layout + metrics are removed on drop. The
/// guard must never be `.keep()`'d, or the workspace leaks into
/// `$TMPDIR`.
fn bench_workspace(
    layout_dir_override: Option<&Path>,
) -> Result<(PathBuf, Option<TempDir>), CliBenchError> {
    match layout_dir_override {
        Some(p) => Ok((p.to_path_buf(), None)),
        None => {
            let td = tempfile::tempdir()?;
            Ok((td.path().to_path_buf(), Some(td)))
        }
    }
}

/// Wipe a layout directory between cold-cache runs. Preserves the
/// directory itself (so subsequent runs find it where they expect).
fn purge_layout(layout_dir: &Path) -> Result<(), CliBenchError> {
    if layout_dir.exists() {
        std::fs::remove_dir_all(layout_dir)?;
    }
    std::fs::create_dir_all(layout_dir)?;
    Ok(())
}

/// Drive one bench iteration: spawn the umf binary as `umf build
/// --metrics=json --metrics-output=<path> ...`, wait, parse the
/// emitted metrics file.
fn run_one_bench_iteration(
    umf_bin: &Path,
    recipe: &Path,
    layout_dir: &Path,
    metrics_dir: &Path,
    tag: &str,
    label: &str,
) -> Result<umf_builder::metrics::BuildMetrics, CliBenchError> {
    let metrics_path = metrics_dir.join(format!("{label}.json"));
    let output = std::process::Command::new(umf_bin)
        .arg("--trace-level=warn")
        .arg("build")
        .arg("--metrics=json")
        .arg("--metrics-output")
        .arg(&metrics_path)
        .arg("--layout-dir")
        .arg(layout_dir)
        .arg("--tag")
        .arg(tag)
        .arg(recipe)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(CliBenchError::BuildFailed {
            run: label.to_string(),
            message: stderr.trim().to_string(),
        });
    }
    let bytes = std::fs::read(&metrics_path)?;
    serde_json::from_slice(&bytes).map_err(|err| CliBenchError::ParseMetrics {
        run: label.to_string(),
        err,
    })
}

#[cfg(test)]
mod tests;
