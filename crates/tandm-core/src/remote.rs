//! Talking to peer machines: `ssh <alias> tandm …`. No daemon, no open port; ssh over the tailnet
//! does auth and transport.

use std::process::Command;
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::config::MachineConfig;
use crate::status::MachineStatus;
use crate::util::{self, shell_quote};

pub const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(180);

/// The remote command line: the configured tandm command (left as-is so `~/…` expands remotely),
/// then each argument quoted for the remote shell.
pub fn remote_command_line(m: &MachineConfig, args: &[&str]) -> String {
    let mut s = m.tandm_cmd().to_string();
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

pub fn ssh_command(m: &MachineConfig, args: &[&str]) -> Command {
    let mut c = Command::new("ssh");
    c.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "LogLevel=ERROR",
        "--",
        m.ssh_target(),
        &remote_command_line(m, args),
    ]);
    c
}

/// Run `tandm <args>` on a peer and return its stdout. Errors read like what went wrong from the
/// user's point of view (unreachable vs. tandm missing vs. the command failing).
pub fn run(m: &MachineConfig, args: &[&str], timeout: Duration) -> Result<String> {
    let out = util::run_with_timeout(ssh_command(m, args), timeout)
        .map_err(|e| anyhow!("ssh {}: {e:#}", m.ssh_target()))?;
    if out.success() {
        return Ok(out.stdout);
    }
    let err = util::excerpt(&out.stderr, 3);
    Err(match out.status.code() {
        Some(255) => anyhow!("unreachable over ssh ({}): {err}", m.ssh_target()),
        Some(127) => anyhow!(
            "tandm not found on {} (set `tandm = \"…\"` for this machine): {err}",
            m.name
        ),
        _ => anyhow!("{err}"),
    })
}

pub fn status(m: &MachineConfig) -> Result<MachineStatus> {
    let out = run(m, &["status", "--json"], STATUS_TIMEOUT)?;
    serde_json::from_str(&out).map_err(|e| anyhow!("bad status JSON from {}: {e}", m.name))
}

/// Query every peer in parallel.
pub fn all_status(machines: &[MachineConfig]) -> Vec<(MachineConfig, Result<MachineStatus>)> {
    std::thread::scope(|s| {
        let hs: Vec<_> = machines
            .iter()
            .map(|m| (m.clone(), s.spawn(move || status(m))))
            .collect();
        hs.into_iter()
            .map(|(m, h)| {
                let r = h
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("status thread panicked")));
                (m, r)
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_quotes_args_not_the_command() {
        let m = MachineConfig {
            name: "mac".into(),
            ssh: None,
            tandm: Some("~/.local/bin/tandm".into()),
        };
        assert_eq!(
            remote_command_line(&m, &["add", "~/projects/a b", "--name", "x"]),
            "~/.local/bin/tandm add '~/projects/a b' --name x"
        );
    }
}
