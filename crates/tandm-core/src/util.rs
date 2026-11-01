use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Write via a temp file + rename so readers (the GUI, a concurrent `status`) never see half a file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Names end up in file names and in remote shell commands, so keep them boring.
pub fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn short_hostname() -> String {
    let full = hostname();
    full.split('.').next().unwrap_or(&full).to_string()
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "localhost".into()
}

#[cfg(not(unix))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}

/// Quote one argument for a POSIX shell (ssh joins its command arguments into one string).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '=' | ':' | ','))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// Run a command to completion, killing it after `timeout`. Output is captured on threads so a
/// chatty child can't fill a pipe and deadlock.
pub fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child: Child = cmd.spawn().with_context(|| format!("starting {cmd:?}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = thread::spawn(move || {
        let mut s = Vec::new();
        out.read_to_end(&mut s).ok();
        s
    });
    let t_err = thread::spawn(move || {
        let mut s = Vec::new();
        err.read_to_end(&mut s).ok();
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            bail!("timed out after {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

/// First few non-empty lines of some command output, for logs and status details.
pub fn excerpt(s: &str, max_lines: usize) -> String {
    s.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("my-repo2"), "my-repo2");
        assert_eq!(shell_quote("~/x y"), "'~/x y'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn names() {
        assert!(is_safe_name("4u-repo"));
        assert!(!is_safe_name("../x"));
        assert!(!is_safe_name("-rf"));
        assert!(!is_safe_name("a b"));
    }
}
