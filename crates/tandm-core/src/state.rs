//! Per-repo files in the state dir: `<name>.json` (last run), `<name>.log`, `<name>.lock`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::secrets::SecretHit;
use crate::util;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunResult {
    /// Nothing to pull or push.
    Synced,
    /// Pulled and/or pushed something.
    Changed,
    Skipped,
    Error,
    Paused,
    /// Written by a newer tandm.
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    NotOnBranch,
    InProgress,
    Overlap,
    RebaseConflict,
    Secrets,
    #[serde(other)]
    Other,
}

/// Outcome of one sync run. Flat, with defaults everywhere, so that machines running different
/// tandm versions can still read each other's `status --json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunRecord {
    /// Unix seconds.
    pub at: i64,
    pub result: RunResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<SkipReason>,
    /// Human-readable, same wording as the old script's log lines.
    pub message: String,
    pub pulled: u32,
    /// Local commits rebased onto upstream in this run.
    pub rebased: u32,
    pub pushed: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overlap: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secret_hits: Vec<SecretHit>,
    /// git's stderr for errors, trimmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Branch, HEAD, ahead/behind and dirty count right after the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<crate::status::LiveStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoState {
    pub last_run: Option<RunRecord>,
    /// Last time this machine pulled or pushed something, and what.
    pub last_change_at: Option<i64>,
    pub last_change: Option<String>,
    /// The skip/error note last written to the log (the script's `<repo>.last`), so a persisting
    /// condition is logged once, not every two minutes.
    pub logged_note: Option<String>,
}

pub struct StateFiles {
    dir: PathBuf,
    name: String,
}

impl StateFiles {
    pub fn new(dir: &Path, name: &str) -> Self {
        Self {
            dir: dir.to_path_buf(),
            name: name.to_string(),
        }
    }

    pub fn json(&self) -> PathBuf {
        self.dir.join(format!("{}.json", self.name))
    }
    pub fn log(&self) -> PathBuf {
        self.dir.join(format!("{}.log", self.name))
    }
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(format!("{}.lock", self.name))
    }

    pub fn load(&self) -> RepoState {
        fs::read(self.json())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, st: &RepoState) -> Result<()> {
        util::write_atomic(&self.json(), &serde_json::to_vec_pretty(st)?)
    }

    /// Non-blocking exclusive lock; `None` if another run holds it. The OS releases it when the
    /// process exits, however it exits, so there is no stale-lock cleanup to do.
    pub fn try_lock(&self) -> Result<Option<RunLock>> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.lock_path())
            .with_context(|| format!("opening {}", self.lock_path().display()))?;
        // A child that another thread is spawning (git, ssh) holds a copy of every fd between fork
        // and exec, which keeps a just-released flock alive for a moment. Retry briefly so that
        // window doesn't read as "another run holds the lock"; a real run holds it for seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(Some(RunLock { _file: f })),
                Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => {
                    return Err(e)
                        .with_context(|| format!("locking {}", self.lock_path().display()));
                }
            }
        }
    }

    pub fn append_log(&self, line: &str) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log())?;
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        for (i, l) in line.lines().enumerate() {
            if i == 0 {
                writeln!(f, "{ts} {l}")?;
            } else {
                writeln!(f, "{ts}   {l}")?;
            }
        }
        // Lines are rare (actions and state changes only); make them survive a crash like the
        // JSON record does.
        f.sync_data().ok();
        Ok(())
    }

    /// Last `n` lines of the log.
    pub fn log_tail(&self, n: usize) -> Vec<String> {
        let Ok(mut f) = File::open(self.log()) else {
            return Vec::new();
        };
        // Logs only grow by state changes, but don't read megabytes if one ever gets big.
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let window = (n as u64).saturating_mul(400).max(64 * 1024);
        if len > window {
            f.seek(SeekFrom::Start(len - window)).ok();
        }
        let mut lines: Vec<String> = BufReader::new(f).lines().map_while(Result::ok).collect();
        if len > window && !lines.is_empty() {
            lines.remove(0); // probably partial
        }
        let skip = lines.len().saturating_sub(n);
        lines.split_off(skip)
    }

    /// Remove the record and lock (the log is kept as history).
    pub fn remove(&self) {
        fs::remove_file(self.json()).ok();
        fs::remove_file(self.lock_path()).ok();
    }
}

pub struct RunLock {
    _file: File,
}
