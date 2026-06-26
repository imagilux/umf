//! Process registry — umf's record of the builds and runs it launches.
//!
//! umf has no daemon, so there's nothing tracking what it has run. Each
//! `umf build` / `umf run` records itself here on start (`running`) and
//! updates the record on exit (`exited` / `failed`); `umf ps` reads it.
//!
//! Records live as one JSON file per process under
//! `$XDG_STATE_HOME/umf/processes/` (falling back to
//! `~/.local/state/umf/processes/`), so history survives across
//! invocations. A process killed hard (SIGKILL, power loss) leaves a
//! stale `running` record; [`ProcessRegistry::list`] reconciles those by
//! checking whether the recorded pid is still alive.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// What kind of umf process a record describes — the `ps` `TYPE` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProcessKind {
    /// A `umf build` invocation.
    Build,
    /// A container `umf run`.
    Container,
    /// A VM `umf run`.
    Vm,
}

impl ProcessKind {
    /// Lower-case label used in output + filters.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Container => "container",
            Self::Vm => "vm",
        }
    }
}

/// Lifecycle status — the `ps` `STATUS` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProcessStatus {
    /// Still executing.
    Running,
    /// Finished; the workload's own exit code is in [`ProcessRecord::exit_code`].
    Exited,
    /// umf itself errored, or the process was killed before a clean exit.
    Failed,
}

impl ProcessStatus {
    /// Lower-case label used in output + filters.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }
}

/// One umf-managed process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProcessRecord {
    /// Short hex id (the `ID` column).
    pub id: String,
    /// Friendly name — the image reference or recipe (the `NAME` column).
    pub name: String,
    /// The operation/command that ran (the `PROCESS` column).
    pub process: String,
    /// Build / container / vm (the `TYPE` column).
    pub kind: ProcessKind,
    /// Lifecycle status (the `STATUS` column).
    pub status: ProcessStatus,
    /// Workload exit code once finished, when known.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Kernel release, for VM runs (the `RELEASE` column).
    #[serde(default)]
    pub release: Option<String>,
    /// OCI reference / disk path the process operated on.
    #[serde(default)]
    pub reference: Option<String>,
    /// PID of the umf process that owns this record (used for liveness
    /// reconciliation of stale `running` records).
    pub pid: i32,
    /// Unix epoch seconds when the process started.
    pub started_epoch: u64,
    /// Unix epoch seconds when it finished, if it has.
    #[serde(default)]
    pub finished_epoch: Option<u64>,
}

/// The on-disk process registry directory.
pub(crate) struct ProcessRegistry {
    dir: PathBuf,
}

impl ProcessRegistry {
    /// Resolve the registry directory (`$XDG_STATE_HOME/umf/processes`,
    /// falling back to `~/.local/state/umf/processes`) and ensure it exists.
    pub(crate) fn open() -> std::io::Result<Self> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "cannot locate state dir: neither XDG_STATE_HOME nor HOME is set",
                )
            })?;
        Self::at(base.join("umf").join("processes"))
    }

    /// Open a registry rooted at an explicit directory (creating it).
    /// [`Self::open`] resolves the standard location and defers here.
    pub(crate) fn at(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write (or overwrite) a record, via a temp-file rename so a reader
    /// never sees a half-written file.
    pub(crate) fn write(&self, record: &ProcessRecord) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.dir.join(format!(".{}.tmp", record.id));
        fs::write(&tmp, &json)?;
        fs::rename(&tmp, self.record_path(&record.id))
    }

    /// Read every record, reconciling stale `running` entries whose owning
    /// pid is gone (a hard-killed process never updated its status) to
    /// `failed`. Unparseable files are skipped rather than fatal.
    pub(crate) fn list(&self) -> std::io::Result<Vec<ProcessRecord>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(mut record) = serde_json::from_slice::<ProcessRecord>(&bytes) else {
                continue;
            };
            if record.status == ProcessStatus::Running && !pid_alive(record.pid) {
                record.status = ProcessStatus::Failed;
                record.finished_epoch.get_or_insert_with(now_epoch);
                let _ = self.write(&record); // persist reconcile, best-effort
            }
            out.push(record);
        }
        Ok(out)
    }

    /// Delete a record by id. A missing record is not an error (prune is
    /// idempotent and tolerant of concurrent invocations).
    pub(crate) fn remove(&self, id: &str) -> std::io::Result<()> {
        match fs::remove_file(self.record_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// RAII recorder: writes a `running` record on construction and updates it
/// to `exited` / `failed` on [`Self::exited`] / [`Self::failed`] or, if
/// neither was called (early `?` return, panic), on drop — so every exit
/// path is captured. Best-effort: if the registry can't be opened or
/// written, the guard degrades to a no-op rather than breaking the
/// build/run it's wrapping.
pub(crate) struct RunningGuard {
    inner: Option<GuardInner>,
}

struct GuardInner {
    registry: ProcessRegistry,
    record: ProcessRecord,
    finished: bool,
}

impl RunningGuard {
    /// Register a `running` process and return a guard that finalises it.
    pub(crate) fn start(
        kind: ProcessKind,
        name: impl Into<String>,
        process: impl Into<String>,
        reference: Option<String>,
        release: Option<String>,
    ) -> Self {
        let inner = (|| {
            let registry = ProcessRegistry::open().ok()?;
            let pid = std::process::id() as i32;
            let started = now_epoch();
            let record = ProcessRecord {
                id: gen_id(pid),
                name: name.into(),
                process: process.into(),
                kind,
                status: ProcessStatus::Running,
                exit_code: None,
                release,
                reference,
                pid,
                started_epoch: started,
                finished_epoch: None,
            };
            registry.write(&record).ok()?;
            Some(GuardInner {
                registry,
                record,
                finished: false,
            })
        })();
        Self { inner }
    }

    /// Mark the process finished with the workload's exit code.
    pub(crate) fn exited(mut self, code: i32) {
        self.finalize(ProcessStatus::Exited, Some(code));
    }

    /// Mark the process failed (umf itself errored before a clean exit).
    pub(crate) fn failed(mut self) {
        self.finalize(ProcessStatus::Failed, None);
    }

    fn finalize(&mut self, status: ProcessStatus, code: Option<i32>) {
        if let Some(inner) = self.inner.as_mut() {
            if inner.finished {
                return;
            }
            inner.record.status = status;
            inner.record.exit_code = code;
            inner.record.finished_epoch = Some(now_epoch());
            let _ = inner.registry.write(&inner.record);
            inner.finished = true;
        }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        // Not explicitly finished ⇒ aborted (early return / panic) ⇒ failed.
        self.finalize(ProcessStatus::Failed, None);
    }
}

/// True if `pid` names a live process (Linux `/proc` check).
fn pid_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// Current Unix epoch in seconds.
pub(crate) fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A short hex id: low 32 bits of the nanosecond clock, the low 16 bits of the
/// pid, and a process-local monotonic counter. The counter makes ids generated
/// within one process collision-free; the clock + pid keep cross-process
/// collisions improbable (they'd need the same nanosecond-low-32 *and* the same
/// pid-low-16 *and* the same counter-low-16).
fn gen_id(pid: i32) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:08x}{:04x}{:04x}",
        (nanos as u64) & 0xffff_ffff,
        (pid as u32) & 0xffff,
        (seq as u32) & 0xffff
    )
}
