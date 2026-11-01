//! The sync engine: a port of the original `git-autosync.sh` bash script, one repo per call.
//!
//! It never commits, never stashes by hand, never force-pushes. Several agent sessions share the
//! worktree, so it backs off whenever touching the tree could hurt their work:
//!   - not on the branch, a merge/rebase/cherry-pick/revert in progress, or index.lock → skip
//!   - uncommitted files overlap what upstream changed → skip (a human merges)
//!   - a rebase of local commits conflicts → abort it, leave everything as it was
//!   - commits about to be pushed contain secret-looking lines → don't push
//!
//! Diverged history is rebased, so local commit ids change after a sync.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::config::RepoConfig;
use crate::git::Git;
use crate::secrets;
use crate::state::{RepoState, RunRecord, RunResult, SkipReason, StateFiles};
use crate::util::{self, excerpt};

/// Files/dirs in the git dir that mean someone is in the middle of something.
pub const IN_PROGRESS_MARKERS: &[&str] = &[
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-merge",
    "rebase-apply",
    "index.lock",
];

/// Sync one repo. Returns `None` when another run holds the repo's lock (nothing was done).
pub fn sync_repo(repo: &RepoConfig, state_dir: &Path) -> Result<Option<RunRecord>> {
    let files = StateFiles::new(state_dir, &repo.name);
    let Some(_lock) = files.try_lock()? else {
        return Ok(None);
    };
    let mut st = files.load();
    let mut rec = if repo.paused {
        RunRecord {
            at: util::now(),
            result: RunResult::Paused,
            message: "paused".into(),
            ..Default::default()
        }
    } else {
        let mut run = Run {
            repo,
            files: &files,
            st: &mut st,
            rec: RunRecord {
                at: util::now(),
                ..Default::default()
            },
            actions: Vec::new(),
        };
        let end = run.steps().unwrap_or_else(|e| End::Stop {
            result: RunResult::Error,
            reason: None,
            msg: format!("error: {e:#}"),
            detail: None,
        });
        run.finish(end)
    };
    if repo.path.is_dir() {
        rec.after = crate::status::live_status(repo).ok();
    }
    st.last_run = Some(rec.clone());
    files.save(&st)?;
    Ok(Some(rec))
}

enum End {
    Done,
    Stop {
        result: RunResult,
        reason: Option<SkipReason>,
        msg: String,
        detail: Option<String>,
    },
}

fn skip(reason: SkipReason, msg: String) -> End {
    End::Stop {
        result: RunResult::Skipped,
        reason: Some(reason),
        msg,
        detail: None,
    }
}

fn error(msg: &str, detail: &str) -> End {
    let d = excerpt(detail, 6);
    End::Stop {
        result: RunResult::Error,
        reason: None,
        msg: msg.to_string(),
        detail: (!d.is_empty()).then_some(d),
    }
}

struct Run<'a> {
    repo: &'a RepoConfig,
    files: &'a StateFiles,
    st: &'a mut RepoState,
    rec: RunRecord,
    actions: Vec<String>,
}

impl Run<'_> {
    /// Actions are always logged.
    fn action(&mut self, msg: String) {
        self.files.append_log(&msg).ok();
        self.actions.push(msg);
    }

    fn steps(&mut self) -> Result<End> {
        let repo = self.repo;
        if !repo.path.is_dir() {
            return Ok(error(
                &format!("repo path missing: {}", repo.path.display()),
                "",
            ));
        }
        let g = Git::new(&repo.path);
        let gd = match g.git_dir() {
            Ok(d) => d,
            Err(e) => return Ok(error("not a git repository", &format!("{e:#}"))),
        };

        if g.current_branch()?.as_deref() != Some(repo.branch.as_str()) {
            return Ok(skip(
                SkipReason::NotOnBranch,
                format!("skip: not on {}", repo.branch),
            ));
        }
        for f in IN_PROGRESS_MARKERS {
            if gd.join(f).exists() {
                return Ok(skip(
                    SkipReason::InProgress,
                    format!("skip: {f} present (git operation in progress)"),
                ));
            }
        }

        let fetch = g.run(&["fetch", "-q", &repo.remote, &repo.branch])?;
        if !fetch.success() {
            return Ok(error("fetch failed", &fetch.stderr));
        }
        // Full ref name, so a local branch literally called "origin/main" can't shadow it.
        let up = format!("refs/remotes/{}/{}", repo.remote, repo.branch);
        if g.rev(&up)?.is_none() {
            return Ok(error(
                &format!("{} not found after fetch", repo.upstream()),
                "",
            ));
        }
        let (ahead, behind) = g.ahead_behind(&up)?;

        if behind > 0 {
            // Files touched upstream since we split vs. files with uncommitted changes (incl.
            // untracked, and both names of a rename).
            let base = g.out(&["merge-base", "HEAD", &up])?;
            let upstream: BTreeSet<String> = g.changed_files(&base, &up)?.into_iter().collect();
            let dirty: BTreeSet<String> = g
                .status_entries()?
                .into_iter()
                .flat_map(|(p, orig)| std::iter::once(p).chain(orig))
                .collect();
            let overlap: Vec<String> = upstream.intersection(&dirty).cloned().collect();
            if !overlap.is_empty() {
                let msg = format!(
                    "skip pull: uncommitted changes overlap upstream: {}",
                    overlap.join(" ")
                );
                self.rec.overlap = overlap;
                return Ok(skip(SkipReason::Overlap, msg));
            }

            if ahead == 0 {
                let m = g.run(&["merge", "-q", "--ff-only", &up])?;
                if !m.success() {
                    return Ok(error(
                        "fast-forward failed",
                        &format!("{}{}", m.stdout, m.stderr),
                    ));
                }
                self.rec.pulled = behind;
                self.action(format!(
                    "pulled {behind} commit(s) -> {}",
                    g.short("HEAD")
                ));
            } else {
                let stash_before = g.rev("refs/stash")?;
                let r = g.run(&["-c", "rebase.autoStash=true", "rebase", "-q", &up])?;
                if !r.success() {
                    let abort = g.run(&["rebase", "--abort"])?;
                    let detail = format!("{}{}", r.stdout, r.stderr);
                    if !abort.success() {
                        return Ok(error(
                            "rebase failed and `git rebase --abort` failed too; fix by hand",
                            &format!("{detail}{}", abort.stderr),
                        ));
                    }
                    return Ok(End::Stop {
                        result: RunResult::Skipped,
                        reason: Some(SkipReason::RebaseConflict),
                        msg: format!(
                            "skip: rebasing {ahead} local commit(s) onto {behind} upstream conflicts; resolve by hand"
                        ),
                        detail: Some(excerpt(&detail, 6)).filter(|d| !d.is_empty()),
                    });
                }
                self.rec.pulled = behind;
                self.rec.rebased = ahead;
                self.action(format!(
                    "rebased {ahead} local commit(s) onto {behind} upstream -> {}",
                    g.short("HEAD")
                ));
                // The rebase worked but the autostash didn't re-apply cleanly: git kept it in the
                // stash list. The overlap check should make this impossible short of a race.
                if g.rev("refs/stash")? != stash_before {
                    return Ok(error(
                        "rebased, but uncommitted changes could not be re-applied: they are safe in `git stash list` (autostash)",
                        &r.stderr,
                    ));
                }
            }
        }

        let (ahead, _) = g.ahead_behind(&up)?;
        if ahead > 0 {
            // Every outgoing commit's patch, not just the net diff: a secret added and removed again
            // locally would still be pushed in history.
            let log = g.out(&[
                "-c",
                "core.quotePath=false",
                "log",
                "-p",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--diff-merges=first-parent",
                "--format=commit %H",
                &format!("{up}..HEAD"),
            ])?;
            let hits = secrets::scan_diff(&log);
            if !hits.is_empty() {
                let shown: Vec<String> = hits.iter().take(3).map(|h| h.to_string()).collect();
                let more = if hits.len() > 3 {
                    format!(" (+{} more)", hits.len() - 3)
                } else {
                    String::new()
                };
                let msg = format!(
                    "NOT pushing {ahead} commit(s): secret-looking lines: {}{more}",
                    shown.join(" ")
                );
                self.rec.secret_hits = hits;
                return Ok(skip(SkipReason::Secrets, msg));
            }
            let p = g.run(&["push", "-q", &self.repo.remote, &self.repo.branch])?;
            if !p.success() {
                return Ok(error("push failed", &p.stderr));
            }
            self.rec.pushed = ahead;
            self.action(format!("pushed {ahead} commit(s) -> {}", g.short("HEAD")));
        }
        Ok(End::Done)
    }

    fn finish(mut self, end: End) -> RunRecord {
        let mut parts = self.actions.clone();
        match end {
            End::Done => {
                if let Some(prev) = self.st.logged_note.take() {
                    self.files.append_log(&format!("ok again (was: {prev})")).ok();
                }
                self.rec.result = if self.actions.is_empty() {
                    RunResult::Synced
                } else {
                    RunResult::Changed
                };
            }
            End::Stop {
                result,
                reason,
                msg,
                detail,
            } => {
                // Log a skip/error once, not every run.
                if self.st.logged_note.as_deref() != Some(msg.as_str()) {
                    let text = match &detail {
                        Some(d) => format!("{msg}\n{d}"),
                        None => msg.clone(),
                    };
                    self.files.append_log(&text).ok();
                    self.st.logged_note = Some(msg.clone());
                }
                self.rec.result = result;
                self.rec.reason = reason;
                self.rec.detail = detail;
                parts.push(msg);
            }
        }
        if !self.actions.is_empty() {
            self.st.last_change_at = Some(self.rec.at);
            self.st.last_change = Some(self.actions.join("; "));
        }
        self.rec.message = if parts.is_empty() {
            "in sync".into()
        } else {
            parts.join("; ")
        };
        self.rec
    }
}

impl RunRecord {
    /// The cases where the script exited non-zero.
    pub fn is_failure(&self) -> bool {
        self.result == RunResult::Error
            || matches!(
                self.reason,
                Some(SkipReason::RebaseConflict | SkipReason::Secrets)
            )
    }
}

impl RepoState {
    pub fn last_change_summary(&self) -> Option<(i64, &str)> {
        Some((self.last_change_at?, self.last_change.as_deref()?))
    }
}
