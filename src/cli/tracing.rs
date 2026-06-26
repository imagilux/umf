//! Tracing subscriber wiring for the `umf` CLI — format selection
//! (`--trace-format`), output routing (`--trace-output`), and level
//! filtering (`--trace-level` / `RUST_LOG`).

use clap::ValueEnum;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// Output format for tracing spans + events. Defaults to `text` so
/// existing tooling that grep-parses the log lines keeps working
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum TraceFormat {
    /// Compact human-readable lines (the previous default).
    Text,
    /// One structured JSON object per event / span — pipeable to
    /// `jq`, Loki, Honeycomb, Datadog.
    Json,
    /// `tracing-subscriber` pretty layer — tree-shaped, multi-line,
    /// best for local debugging.
    Pretty,
}

/// Install the global tracing subscriber.
///
/// Honors three CLI surfaces (in order of precedence for each concern):
///
/// - **Filter**: `RUST_LOG` (env, wins if present) → `--trace-level` →
///   `info` default.
/// - **Format**: `--trace-format` (`text`/`json`/`pretty`), defaults
///   to `text` to preserve today's user-visible output shape.
/// - **Output**: `--trace-output` (`stderr`/`stdout`/`<path>`),
///   defaults to `stderr`.
///
/// Idempotent across re-init attempts in the same process (the
/// subscriber is set once; subsequent attempts are no-ops).
pub(crate) fn setup_tracing(
    format: TraceFormat,
    output: Option<&str>,
    level: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => {
            let directive = level.unwrap_or("info");
            EnvFilter::try_new(directive)?
        }
    };

    let writer = open_trace_writer(output)?;

    // Each format variant builds a complete subscriber and installs
    // it. We can't reuse a single fmt::Subscriber across variants
    // because `.json()` / `.pretty()` change the type — installing
    // separately is the idiomatic dance.
    //
    // For json/pretty we ask for NEW + CLOSE span events so a
    // structured consumer sees `enter span umf.engine.run_step ... close`
    // pairs and can compute per-span timings without needing to
    // tail every event. Text format keeps today's behaviour (events
    // only) to avoid log churn for users on the default format.
    match format {
        TraceFormat::Text => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(writer)
                .init();
        }
        TraceFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_env_filter(env_filter)
                .with_writer(writer)
                .init();
        }
        TraceFormat::Pretty => {
            tracing_subscriber::fmt()
                .pretty()
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_env_filter(env_filter)
                .with_writer(writer)
                .init();
        }
    }
    Ok(())
}

/// Type alias for the trace-writer factory the subscriber keeps. Lives
/// outside [`open_trace_writer`] so the signature stays readable
/// (clippy's `type_complexity` lint refuses the inline form).
pub(crate) type WriterFactory =
    Box<dyn Fn() -> Box<dyn std::io::Write + Send + 'static> + Send + Sync>;
type BoxedError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Resolve `--trace-output` to a writer factory the subscriber can
/// keep around. `stderr` / `stdout` are special; anything else is
/// treated as a file path opened in append mode (created if missing).
pub(crate) fn open_trace_writer(output: Option<&str>) -> Result<WriterFactory, BoxedError> {
    match output {
        None | Some("stderr") => Ok(Box::new(|| -> Box<dyn std::io::Write + Send + 'static> {
            Box::new(std::io::stderr())
        })),
        Some("stdout") => Ok(Box::new(|| -> Box<dyn std::io::Write + Send + 'static> {
            Box::new(std::io::stdout())
        })),
        Some(path) => {
            // Confirm we can open the path up front so the error
            // surfaces from `setup_tracing` rather than from the
            // first log line (which would happen after the
            // subscriber claimed success).
            let probe = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("--trace-output {path:?}: {e}"))?;
            drop(probe);
            let path = path.to_string();
            Ok(Box::new(
                move || -> Box<dyn std::io::Write + Send + 'static> {
                    // tracing-subscriber calls the factory once per
                    // write batch. If the file becomes unwriteable
                    // mid-run (disk full / unmounted) we fall back to
                    // stderr so the run keeps going — losing
                    // structured traces is preferable to panicking.
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        Ok(f) => Box::new(f),
                        Err(_) => Box::new(std::io::stderr()),
                    }
                },
            ))
        }
    }
}
