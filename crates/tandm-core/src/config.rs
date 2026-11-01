//! `config.toml`: the repos this machine syncs and the peer machines the GUI shows.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

pub const DEFAULT_INTERVAL_SECS: u64 = 120;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// How often the service runs `tandm sync`.
    pub interval_secs: u64,
    /// Name this machine reports; defaults to the short hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_name: Option<String>,
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
    #[serde(rename = "machine")]
    pub machines: Vec<MachineConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
            machine_name: None,
            repos: Vec::new(),
            machines: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoConfig {
    /// Row key across machines (the same repo has different paths on Linux and macOS).
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

fn default_branch() -> String {
    "main".into()
}
fn default_remote() -> String {
    "origin".into()
}

impl RepoConfig {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            branch: default_branch(),
            remote: default_remote(),
            paused: false,
        }
    }

    pub fn upstream(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineConfig {
    /// Column title in the GUI.
    pub name: String,
    /// ssh destination (alias from ~/.ssh/config, tailnet name, user@host…). Defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// Command that runs tandm on that machine, e.g. `~/.local/bin/tandm`. Defaults to `tandm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tandm: Option<String>,
}

impl MachineConfig {
    pub fn ssh_target(&self) -> &str {
        self.ssh.as_deref().unwrap_or(&self.name)
    }
    pub fn tandm_cmd(&self) -> &str {
        self.tandm.as_deref().unwrap_or("tandm")
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&paths::config_file())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let c: Config =
                    toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
                c.validate()?;
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&paths::config_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let body = toml::to_string_pretty(self)?;
        let text = format!(
            "# tandm config — edit freely, or use `tandm add/rm/pause/resume/machine`.\n{body}"
        );
        crate::util::write_atomic(path, text.as_bytes())
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for r in &self.repos {
            if !crate::util::is_safe_name(&r.name) {
                bail!("repo name {:?} must be letters, digits, '.', '_' or '-'", r.name);
            }
            if !seen.insert(r.name.as_str()) {
                bail!("repo name {:?} is used twice in the config", r.name);
            }
        }
        Ok(())
    }

    pub fn machine_name(&self) -> String {
        self.machine_name
            .clone()
            .unwrap_or_else(crate::util::short_hostname)
    }

    /// Find a repo by name, or by path (absolute, `~/…`, or relative to the cwd).
    pub fn find_repo(&self, key: &str) -> Option<usize> {
        if let Some(i) = self.repos.iter().position(|r| r.name == key) {
            return Some(i);
        }
        let p = paths::expand_tilde(key);
        let p = fs::canonicalize(&p).unwrap_or(p);
        self.repos
            .iter()
            .position(|r| fs::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone()) == p)
    }

    pub fn repo(&self, key: &str) -> Result<&RepoConfig> {
        match self.find_repo(key) {
            Some(i) => Ok(&self.repos[i]),
            None => bail!("no repo {key:?} in tandm (see `tandm list`)"),
        }
    }
}
