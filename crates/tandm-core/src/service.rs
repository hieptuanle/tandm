//! `tandm service install|uninstall|status`: one background job per machine that runs
//! `tandm sync --quiet` for every repo every `interval_secs`.
//! Linux: systemd user timer. macOS: launchd LaunchAgent. Windows: not yet.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::util::{self, run_with_timeout};

pub const SYSTEMD_UNIT: &str = "tandm";
pub const LAUNCHD_LABEL: &str = "local.tandm.sync";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceState {
    /// "systemd", "launchd" or "unsupported".
    pub kind: String,
    pub installed: bool,
    pub active: bool,
    pub detail: String,
}

fn systemd_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".config"))
        .join("systemd")
        .join("user")
}

fn launchd_plist() -> PathBuf {
    paths::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

fn quick(cmd: &str, args: &[&str]) -> Result<util::Captured> {
    let mut c = Command::new(cmd);
    c.args(args);
    run_with_timeout(c, Duration::from_secs(15))
}

fn checked(cmd: &str, args: &[&str]) -> Result<()> {
    let c = quick(cmd, args)?;
    if !c.success() {
        bail!(
            "{cmd} {} failed: {}",
            args.join(" "),
            util::excerpt(&format!("{}{}", c.stderr, c.stdout), 4)
        );
    }
    Ok(())
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}
#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

pub fn systemd_service_unit(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    let exe = if exe.contains(char::is_whitespace) {
        format!("\"{exe}\"")
    } else {
        exe.into_owned()
    };
    format!(
        "# Written by `tandm service install`.\n\
         [Unit]\n\
         Description=tandm: keep git repos in sync (see `tandm list`)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} sync --quiet\n"
    )
}

pub fn systemd_timer_unit(interval_secs: u64) -> String {
    format!(
        "# Written by `tandm service install`.\n\
         [Unit]\n\
         Description=tandm sync every {interval_secs}s\n\
         \n\
         [Timer]\n\
         OnBootSec=1min\n\
         OnUnitActiveSec={interval_secs}s\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn launchd_plist_body(exe: &Path, interval_secs: u64, log: &Path) -> String {
    let bin_dir = exe
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path = format!("{bin_dir}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `tandm service install`. -->
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>sync</string><string>--quiet</string></array>
  <key>StartInterval</key><integer>{interval_secs}</integer>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>{path}</string>
  </dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
        label = LAUNCHD_LABEL,
        exe = xml(&exe.to_string_lossy()),
        path = xml(&path),
        log = xml(&log.to_string_lossy()),
    )
}

/// Write the unit(s) and start them. Returns a human summary.
pub fn install(exe: &Path, interval_secs: u64) -> Result<String> {
    let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let mut warn = String::new();
    let s = exe.to_string_lossy();
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        warn = format!(
            "\nwarning: the service points at a build-dir binary ({s}); install tandm first (see README) and re-run `tandm service install` from there"
        );
    }
    match std::env::consts::OS {
        "linux" => {
            let d = systemd_dir();
            fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
            util::write_atomic(
                &d.join(format!("{SYSTEMD_UNIT}.service")),
                systemd_service_unit(&exe).as_bytes(),
            )?;
            util::write_atomic(
                &d.join(format!("{SYSTEMD_UNIT}.timer")),
                systemd_timer_unit(interval_secs).as_bytes(),
            )?;
            checked("systemctl", &["--user", "daemon-reload"])?;
            checked(
                "systemctl",
                &["--user", "enable", "--now", &format!("{SYSTEMD_UNIT}.timer")],
            )?;
            Ok(format!(
                "installed systemd user timer {SYSTEMD_UNIT}.timer (every {interval_secs}s) -> {}{warn}",
                exe.display()
            ))
        }
        "macos" => {
            let p = launchd_plist();
            let domain = format!("gui/{}", uid());
            quick("launchctl", &["bootout", &domain, &p.to_string_lossy()]).ok();
            let log = paths::state_dir().join("service.log");
            fs::create_dir_all(paths::state_dir())?;
            util::write_atomic(&p, launchd_plist_body(&exe, interval_secs, &log).as_bytes())?;
            checked("launchctl", &["bootstrap", &domain, &p.to_string_lossy()])?;
            Ok(format!(
                "installed LaunchAgent {LAUNCHD_LABEL} (every {interval_secs}s) -> {}{warn}",
                exe.display()
            ))
        }
        os => bail!("`tandm service` isn't implemented on {os} yet; run `tandm sync` from a scheduler"),
    }
}

pub fn uninstall() -> Result<String> {
    match std::env::consts::OS {
        "linux" => {
            let d = systemd_dir();
            quick(
                "systemctl",
                &["--user", "disable", "--now", &format!("{SYSTEMD_UNIT}.timer")],
            )
            .ok();
            fs::remove_file(d.join(format!("{SYSTEMD_UNIT}.service"))).ok();
            fs::remove_file(d.join(format!("{SYSTEMD_UNIT}.timer"))).ok();
            checked("systemctl", &["--user", "daemon-reload"])?;
            Ok(format!("removed systemd user timer {SYSTEMD_UNIT}.timer"))
        }
        "macos" => {
            let p = launchd_plist();
            quick(
                "launchctl",
                &["bootout", &format!("gui/{}", uid()), &p.to_string_lossy()],
            )
            .ok();
            fs::remove_file(&p).ok();
            Ok(format!("removed LaunchAgent {LAUNCHD_LABEL}"))
        }
        os => bail!("`tandm service` isn't implemented on {os} yet"),
    }
}

pub fn state() -> ServiceState {
    match std::env::consts::OS {
        "linux" => {
            let d = systemd_dir();
            let installed = d.join(format!("{SYSTEMD_UNIT}.timer")).exists();
            let active = quick(
                "systemctl",
                &["--user", "is-active", &format!("{SYSTEMD_UNIT}.timer")],
            )
            .map(|c| c.stdout.trim().to_string())
            .unwrap_or_else(|e| format!("{e:#}"));
            ServiceState {
                kind: "systemd".into(),
                installed,
                active: active == "active",
                detail: format!("{SYSTEMD_UNIT}.timer: {active}"),
            }
        }
        "macos" => {
            let installed = launchd_plist().exists();
            let loaded = quick(
                "launchctl",
                &["print", &format!("gui/{}/{LAUNCHD_LABEL}", uid())],
            )
            .map(|c| c.success())
            .unwrap_or(false);
            ServiceState {
                kind: "launchd".into(),
                installed,
                active: loaded,
                detail: format!(
                    "{LAUNCHD_LABEL}: {}",
                    if loaded { "loaded" } else { "not loaded" }
                ),
            }
        }
        os => ServiceState {
            kind: "unsupported".into(),
            installed: false,
            active: false,
            detail: format!("no service support on {os} yet"),
        },
    }
}
