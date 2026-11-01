//! Where tandm keeps its config and state on each OS.
//!
//! Tests (and anyone who wants an isolated setup) can point both somewhere else with
//! `TANDM_CONFIG_DIR` / `TANDM_STATE_DIR`.

use std::env;
use std::path::PathBuf;

fn home() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn home_dir() -> PathBuf {
    home()
}

fn non_empty(var: &str) -> Option<PathBuf> {
    env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    if let Some(d) = non_empty("TANDM_CONFIG_DIR") {
        return d;
    }
    if cfg!(windows) {
        return non_empty("APPDATA").unwrap_or_else(home).join("tandm");
    }
    non_empty("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home().join(".config"))
        .join("tandm")
}

pub fn state_dir() -> PathBuf {
    if let Some(d) = non_empty("TANDM_STATE_DIR") {
        return d;
    }
    if cfg!(windows) {
        return non_empty("LOCALAPPDATA").unwrap_or_else(home).join("tandm");
    }
    non_empty("XDG_STATE_HOME")
        .unwrap_or_else(|| home().join(".local").join("state"))
        .join("tandm")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Expand a leading `~` or `~/` to the home directory. Paths typed in the GUI for a remote machine
/// arrive single-quoted over ssh, so the remote shell never expands them; tandm does.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// The reverse, for display and for suggesting the same path on another machine.
pub fn contract_tilde(p: &str) -> String {
    let h = home();
    let h = h.to_string_lossy();
    if !h.is_empty() && h != "/" {
        if p == h {
            return "~".into();
        }
        if let Some(rest) = p.strip_prefix(&format!("{h}/")) {
            return format!("~/{rest}");
        }
    }
    p.to_string()
}
