//! Operations shared by the CLI and the GUI (for this machine; peers get the same via ssh).

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::config::{Config, RepoConfig};
use crate::engine;
use crate::git::Git;
use crate::paths;
use crate::state::{RunRecord, StateFiles};
use crate::util;

pub struct Added {
    pub repo: RepoConfig,
    pub warnings: Vec<String>,
}

pub fn add_repo(
    cfg: &mut Config,
    path: &str,
    name: Option<&str>,
    branch: Option<&str>,
    remote: Option<&str>,
) -> Result<Added> {
    let p = paths::expand_tilde(path);
    let p = std::fs::canonicalize(&p).with_context(|| format!("{}: no such directory", p.display()))?;
    let top = Git::new(&p)
        .out(&["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not inside a git work tree", p.display()))?;
    let top = std::fs::canonicalize(&top).unwrap_or_else(|_| top.into());
    let name = match name {
        Some(n) => n.to_string(),
        None => top
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .context("repo path has no directory name; pass --name")?,
    };
    if !util::is_safe_name(&name) {
        bail!("repo name {name:?} must be letters, digits, '.', '_' or '-'; pass --name");
    }
    if cfg.repos.iter().any(|r| r.name == name) {
        bail!("a repo named {name:?} is already in tandm; pass --name to pick another");
    }
    if let Some(r) = cfg
        .repos
        .iter()
        .find(|r| std::fs::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone()) == top)
    {
        bail!("{} is already in tandm as {:?}", top.display(), r.name);
    }
    let mut repo = RepoConfig::new(name, top);
    if let Some(b) = branch {
        repo.branch = b.to_string();
    }
    if let Some(r) = remote {
        repo.remote = r.to_string();
    }
    let g = Git::new(&repo.path);
    g.out(&["remote", "get-url", &repo.remote])
        .with_context(|| format!("repo has no remote {:?}", repo.remote))?;
    let mut warnings = Vec::new();
    if g.current_branch()?.as_deref() != Some(repo.branch.as_str()) {
        warnings.push(format!(
            "not on {} right now; tandm skips the repo until it is",
            repo.branch
        ));
    }
    cfg.repos.push(repo.clone());
    Ok(Added { repo, warnings })
}

pub fn remove_repo(cfg: &mut Config, key: &str, state_dir: &Path) -> Result<RepoConfig> {
    let i = cfg
        .find_repo(key)
        .with_context(|| format!("no repo {key:?} in tandm (see `tandm list`)"))?;
    let r = cfg.repos.remove(i);
    let files = StateFiles::new(state_dir, &r.name);
    files.append_log("removed from tandm").ok();
    files.remove();
    Ok(r)
}

pub fn set_paused(cfg: &mut Config, key: &str, paused: bool, state_dir: &Path) -> Result<bool> {
    let i = cfg
        .find_repo(key)
        .with_context(|| format!("no repo {key:?} in tandm (see `tandm list`)"))?;
    let r = &mut cfg.repos[i];
    if r.paused == paused {
        return Ok(false);
    }
    r.paused = paused;
    StateFiles::new(state_dir, &r.name)
        .append_log(if paused { "paused by user" } else { "resumed by user" })
        .ok();
    Ok(true)
}

/// Sync one repo (by name/path) or all of them, in parallel. `None` = another run held the lock.
pub fn sync(
    cfg: &Config,
    which: Option<&str>,
    state_dir: &Path,
) -> Result<Vec<(String, Result<Option<RunRecord>>)>> {
    let repos: Vec<&RepoConfig> = match which {
        Some(k) => vec![cfg.repo(k)?],
        None => cfg.repos.iter().collect(),
    };
    Ok(std::thread::scope(|s| {
        let hs: Vec<_> = repos
            .iter()
            .map(|r| (r.name.clone(), s.spawn(move || engine::sync_repo(r, state_dir))))
            .collect();
        hs.into_iter()
            .map(|(n, h)| {
                (
                    n,
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("sync thread panicked"))),
                )
            })
            .collect()
    }))
}
