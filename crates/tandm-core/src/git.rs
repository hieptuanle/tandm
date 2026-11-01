//! Thin wrapper over the `git` CLI. tandm shells out instead of linking a git library, so it does
//! exactly what the old bash script did, with the user's own ssh config, hooks and credentials.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::util::{self, Captured};

/// Long enough for a slow push; short enough that a wedged run doesn't hold the repo lock forever
/// (the script dropped stale locks after 10 minutes).
const GIT_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Git {
    dir: PathBuf,
}

impl Git {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(&self.dir).args(args);
        // Same as the script: never wait for a password or a host-key prompt.
        c.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15",
        );
        c.env("GIT_TERMINAL_PROMPT", "0");
        // Read-only commands (status!) must not grab index.lock to refresh stat info: a parallel
        // agent's `git commit` would fail on it. Mandatory locks (merge, rebase) still apply.
        c.env("GIT_OPTIONAL_LOCKS", "0");
        // No editor can ever open in a background service.
        c.env("GIT_EDITOR", ":");
        c.env("GIT_MERGE_AUTOEDIT", "no");
        c
    }

    /// Run git, capturing output; a non-zero exit is not an error here.
    pub fn run(&self, args: &[&str]) -> Result<Captured> {
        util::run_with_timeout(self.command(args), GIT_TIMEOUT)
    }

    /// Run git and return trimmed stdout, failing with git's stderr if it exits non-zero.
    pub fn out(&self, args: &[&str]) -> Result<String> {
        let c = self.run(args)?;
        if !c.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                util::excerpt(&c.stderr, 3)
            );
        }
        Ok(c.stdout.trim_end_matches(['\n', '\r']).to_string())
    }

    /// Like `out`, but `None` when git exits non-zero (e.g. `symbolic-ref -q` on a detached HEAD).
    pub fn opt(&self, args: &[&str]) -> Result<Option<String>> {
        let c = self.run(args)?;
        Ok(c.success()
            .then(|| c.stdout.trim_end_matches(['\n', '\r']).to_string())
            .filter(|s| !s.is_empty()))
    }

    pub fn git_dir(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(self.out(&["rev-parse", "--absolute-git-dir"])?))
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        self.opt(&["symbolic-ref", "--short", "-q", "HEAD"])
    }

    pub fn rev(&self, rev: &str) -> Result<Option<String>> {
        self.opt(&["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")])
    }

    pub fn short(&self, rev: &str) -> String {
        self.opt(&["rev-parse", "--short", rev])
            .ok()
            .flatten()
            .unwrap_or_else(|| "?".into())
    }

    /// (ahead, behind) of HEAD relative to `upstream`.
    pub fn ahead_behind(&self, upstream: &str) -> Result<(u32, u32)> {
        let s = self.out(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{upstream}...HEAD"),
        ])?;
        let mut it = s.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
        let behind = it.next().unwrap_or(0);
        let ahead = it.next().unwrap_or(0);
        Ok((ahead, behind))
    }

    /// Uncommitted changes, untracked files listed one by one (not as `dir/`). Each entry is
    /// (path, original path of a rename/copy).
    pub fn status_entries(&self) -> Result<Vec<(String, Option<String>)>> {
        let s = self.out(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        Ok(parse_porcelain_z(&s))
    }

    /// Files touched between two commits, renames split into delete + add.
    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let s = self.out(&["diff", "--name-only", "--no-renames", "-z", from, to])?;
        Ok(s.split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect())
    }
}

/// Parse `git status --porcelain=v1 -z`: entries are `XY path\0`, and a rename or copy is followed
/// by its original path as an extra `\0`-terminated field.
pub fn parse_porcelain_z(s: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut fields = s.split('\0');
    while let Some(entry) = fields.next() {
        if entry.len() < 4 || !entry.is_char_boundary(3) {
            continue;
        }
        let (xy, path) = entry.split_at(3);
        let mut code = xy.chars();
        let (x, y) = (code.next().unwrap_or(' '), code.next().unwrap_or(' '));
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            fields.next().map(str::to_string)
        } else {
            None
        };
        out.push((path.to_string(), orig));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain() {
        let s = " M a.txt\0R  new.txt\0old.txt\0?? dir/x y.txt\0";
        let got = parse_porcelain_z(s);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], ("a.txt".into(), None));
        assert_eq!(got[1], ("new.txt".into(), Some("old.txt".into())));
        assert_eq!(got[2], ("dir/x y.txt".into(), None));
    }
}
