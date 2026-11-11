//! Reusable bounded Windows process supervision (Lattice spec §9, §15).
//!
//! Generic worker lifecycle for NAI services: spawn a child process with real
//! native controls, containing it in a Windows Job Object, capturing output,
//! applying timeouts and resource limits, and producing an exit account with
//! provenance for the canonical ledger.
//!
//! This lives in NAI-Lattice (not Gas Town) so future NAI services reuse the
//! same supervisor instead of re-implementing process hosting.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub mod child;

/// Resource limits applied to a supervised worker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerLimits {
    /// Kill the worker (and on Windows its job) after this wall-clock deadline.
    pub timeout: Duration,
    /// Approximate max resident bytes (best-effort where OS supports it).
    pub max_memory_bytes: Option<u64>,
    /// Max allowed wall-clock idle with no output before kill.
    pub idle_timeout: Option<Duration>,
}

impl Default for WorkerLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(600),
            max_memory_bytes: None,
            idle_timeout: None,
        }
    }
}

/// A command to supervise.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProcessSpec {
    /// Program to execute (absolute path preferred; verified on launch).
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// Do not allocate a console window on `cfg(windows)` (CREATE_NO_WINDOW).
    pub no_console: bool,
}

impl ProcessSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            no_console: true,
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args(mut self, a: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(a.into_iter().map(Into::into));
        self
    }
    pub fn cwd(mut self, c: impl Into<PathBuf>) -> Self {
        self.cwd = Some(c.into());
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }
}

/// The outcome of a supervised run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerOutcome {
    pub worker_id: String,
    pub pid: u32,
    pub start_unix_ms: i64,
    pub exit_status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub wall_ms: u64,
    pub was_timed_out: bool,
}

/// Exit status of a supervised worker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ExitStatus {
    Success,
    ExitCode(i32),
    Terminated,
    TimedOut,
    SpawnFailed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("worker timed out after {0:?}")]
    Timeout(Duration),
    #[error("job object: {0}")]
    Job(String),
    #[error("terminate: {0}")]
    Terminate(String),
}