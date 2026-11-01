//! What `tandm status --json` reports, and how a grid cell is derived from it. The GUI on every
//! machine parses this, possibly from a different tandm version, so every field has a default.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{Config, RepoConfig};
use crate::engine::IN_PROGRESS_MARKERS;
use crate::git::Git;
use crate::service::{self, ServiceState};
use crate::state::{RunRecord, RunResult, SkipReason, StateFiles};
use crate::util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineStatus {
    pub machine: String,
    pub os: String,
    pub tandm_version: String,
    /// Unix seconds on that machine, to spot clock skew.
    pub now: i64,
    pub service: ServiceState,
    pub repos: Vec<RepoStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoStatus {
    pub name: String,
    pub path: String,
    pub branch: String,
    pub remote: String,
    pub paused: bool,
    pub last_run: Option<RunRecord>,
    pub last_change_at: Option<i64>,
    pub last_change: Option<String>,
    /// Read from the working tree at status time, no network.
    pub live: Option<LiveStatus>,
    pub live_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveStatus {
    /// Checked-out branch; `None` when detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    /// `<remote>/<branch>` as of the last fetch.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: u32,
    pub in_progress: Option<String>,
}

impl LiveStatus {
    pub fn head_matches_upstream(&self) -> bool {
        self.head.is_some() && self.head == self.upstream
    }
}

pub fn live_status(repo: &RepoConfig) -> Result<LiveStatus> {
    let g = Git::new(&repo.path);
    let gd = g.git_dir()?;
    let up = format!("refs/remotes/{}/{}", repo.remote, repo.branch);
    let upstream = g.rev(&up)?;
    let (ahead, behind) = if upstream.is_some() {
        g.ahead_behind(&up)?
    } else {
        (0, 0)
    };
    Ok(LiveStatus {
        branch: g.current_branch()?,
        head: g.rev("HEAD")?,
        upstream,
        ahead,
        behind,
        dirty: g.status_entries()?.len() as u32,
        in_progress: IN_PROGRESS_MARKERS
            .iter()
            .find(|f| gd.join(f).exists())
            .map(|f| f.to_string()),
    })
}

pub fn repo_status(repo: &RepoConfig, state_dir: &Path) -> RepoStatus {
    let st = StateFiles::new(state_dir, &repo.name).load();
    let (live, live_error) = if repo.path.is_dir() {
        match live_status(repo) {
            Ok(l) => (Some(l), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        }
    } else {
        (None, Some(format!("path missing: {}", repo.path.display())))
    };
    RepoStatus {
        name: repo.name.clone(),
        path: repo.path.to_string_lossy().into_owned(),
        branch: repo.branch.clone(),
        remote: repo.remote.clone(),
        paused: repo.paused,
        last_run: st.last_run,
        last_change_at: st.last_change_at,
        last_change: st.last_change,
        live,
        live_error,
    }
}

pub fn local_status(cfg: &Config, state_dir: &Path) -> MachineStatus {
    let repos = std::thread::scope(|s| {
        let handles: Vec<_> = cfg
            .repos
            .iter()
            .map(|r| s.spawn(move || repo_status(r, state_dir)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    MachineStatus {
        machine: cfg.machine_name(),
        os: std::env::consts::OS.into(),
        tandm_version: VERSION.into(),
        now: util::now(),
        service: service::state(),
        repos,
    }
}

/// One grid cell's headline state, most important first.
#[derive(Debug, Clone, PartialEq)]
pub enum CellState {
    Unreachable(String),
    NotAdded,
    Error(String),
    Paused,
    Skipped(String),
    Diverged { ahead: u32, behind: u32 },
    Behind(u32),
    Ahead(u32),
    InSync,
    /// Added but never run yet and nothing to show.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Busy,
    Warn,
    Bad,
    Muted,
}

impl CellState {
    pub fn of(r: &RepoStatus) -> CellState {
        if let Some(e) = &r.live_error {
            return CellState::Error(e.clone());
        }
        if let Some(run) = &r.last_run {
            if run.result == RunResult::Error {
                return CellState::Error(run.message.clone());
            }
        }
        if r.paused {
            return CellState::Paused;
        }
        if let Some(l) = &r.live {
            if let Some(op) = &l.in_progress {
                return CellState::Skipped(format!("{op} present (git operation in progress)"));
            }
            if l.branch.as_deref() != Some(r.branch.as_str()) {
                return CellState::Skipped(match &l.branch {
                    Some(b) => format!("not on {} (on {b})", r.branch),
                    None => format!("not on {} (detached HEAD)", r.branch),
                });
            }
        }
        if let Some(run) = &r.last_run {
            if run.result == RunResult::Skipped
                && !matches!(
                    run.reason,
                    Some(SkipReason::NotOnBranch | SkipReason::InProgress)
                )
            {
                return CellState::Skipped(short_reason(run));
            }
        }
        match &r.live {
            Some(l) if l.ahead > 0 && l.behind > 0 => CellState::Diverged {
                ahead: l.ahead,
                behind: l.behind,
            },
            Some(l) if l.behind > 0 => CellState::Behind(l.behind),
            Some(l) if l.ahead > 0 => CellState::Ahead(l.ahead),
            Some(_) => CellState::InSync,
            None => CellState::Unknown,
        }
    }

    pub fn label(&self) -> String {
        match self {
            CellState::Unreachable(_) => "unreachable".into(),
            CellState::NotAdded => "not added".into(),
            CellState::Error(_) => "error".into(),
            CellState::Paused => "paused".into(),
            CellState::Skipped(r) => format!("skipped: {r}"),
            CellState::Diverged { ahead, behind } => format!("diverged +{ahead} −{behind}"),
            CellState::Behind(n) => format!("behind {n}"),
            CellState::Ahead(n) => format!("ahead {n}"),
            CellState::InSync => "in sync".into(),
            CellState::Unknown => "no run yet".into(),
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            CellState::InSync => Tone::Good,
            CellState::Ahead(_) | CellState::Behind(_) => Tone::Busy,
            CellState::Diverged { .. } | CellState::Skipped(_) => Tone::Warn,
            CellState::Error(_) | CellState::Unreachable(_) => Tone::Bad,
            CellState::Paused | CellState::NotAdded | CellState::Unknown => Tone::Muted,
        }
    }
}

/// Compact skip reason for a cell; the full message is in the details.
pub fn short_reason(run: &RunRecord) -> String {
    match run.reason {
        Some(SkipReason::Overlap) => format!("{} file(s) overlap upstream", run.overlap.len()),
        Some(SkipReason::RebaseConflict) => "rebase conflicts".into(),
        Some(SkipReason::Secrets) => "push blocked: secret-looking lines".into(),
        Some(SkipReason::NotOnBranch) => "not on branch".into(),
        Some(SkipReason::InProgress) => "git operation in progress".into(),
        _ => run
            .message
            .strip_prefix("skip: ")
            .unwrap_or(&run.message)
            .to_string(),
    }
}

/// "12s ago", "5m ago", "3h ago", "2d ago".
pub fn ago(then: i64, now: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

pub fn short_sha(s: &Option<String>) -> String {
    s.as_deref()
        .map(|h| h.chars().take(7).collect())
        .unwrap_or_else(|| "–".into())
}
