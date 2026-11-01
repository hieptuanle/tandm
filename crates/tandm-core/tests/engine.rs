//! The engine's safety rules, against real git: a bare `origin` plus two clones `a` and `b`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};

use tandm_core::config::RepoConfig;
use tandm_core::engine::sync_repo;
use tandm_core::state::{RunRecord, RunResult, SkipReason, StateFiles};

fn isolate_git() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: runs once, before any test in this binary spawns git or reads the environment.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::set_var("GIT_AUTHOR_NAME", "tandm test");
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@tandm.invalid");
            std::env::set_var("GIT_COMMITTER_NAME", "tandm test");
            std::env::set_var("GIT_COMMITTER_EMAIL", "test@tandm.invalid");
            std::env::set_var("LC_ALL", "C");
        }
    });
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    root: PathBuf,
    origin: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::env::var_os("TANDM_KEEP_TEST_DIRS").is_none() {
            fs::remove_dir_all(&self.root).ok();
        }
    }
}

impl Fixture {
    fn new() -> Fixture {
        isolate_git();
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "tandm-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let origin = root.join("origin.git");
        git(&root, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        let a = root.join("a");
        let b = root.join("b");
        git(&root, &["clone", "-q", origin.to_str().unwrap(), a.to_str().unwrap()]);
        git(&a, &["checkout", "-q", "-b", "main"]);
        fs::write(a.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(a.join("g.txt"), "g\n").unwrap();
        git(&a, &["add", "."]);
        git(&a, &["commit", "-q", "-m", "init"]);
        git(&a, &["push", "-q", "-u", "origin", "main"]);
        git(&root, &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()]);
        Fixture { root, origin, a, b }
    }

    fn state(&self, which: &str) -> PathBuf {
        self.root.join(format!("state-{which}"))
    }

    fn cfg(&self, dir: &Path) -> RepoConfig {
        RepoConfig::new("r", dir)
    }

    /// Run the engine on a clone (`a` or `b`); its state dir is per clone.
    fn sync(&self, dir: &Path) -> RunRecord {
        let which = dir.file_name().unwrap().to_str().unwrap();
        sync_repo(&self.cfg(dir), &self.state(which))
            .expect("sync_repo")
            .expect("lock was free")
    }

    fn log(&self, dir: &Path) -> String {
        let which = dir.file_name().unwrap().to_str().unwrap();
        fs::read_to_string(StateFiles::new(&self.state(which), "r").log()).unwrap_or_default()
    }

    fn json(&self, dir: &Path) -> String {
        let which = dir.file_name().unwrap().to_str().unwrap();
        fs::read_to_string(StateFiles::new(&self.state(which), "r").json()).unwrap_or_default()
    }

    fn origin_head(&self) -> String {
        git(&self.origin, &["rev-parse", "main"])
    }
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"])
}

fn commit(dir: &Path, file: &str, content: &str, msg: &str) -> String {
    let p = dir.join(file);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", msg]);
    head(dir)
}

/// Commit on `b` and push it straight to origin, as another machine would.
fn upstream_commit(fx: &Fixture, file: &str, content: &str) -> String {
    let h = commit(&fx.b, file, content, &format!("upstream {file}"));
    git(&fx.b, &["push", "-q", "origin", "main"]);
    h
}

#[test]
fn in_sync_does_nothing() {
    let fx = Fixture::new();
    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Synced, "{r:?}");
    assert_eq!(r.message, "in sync");
    assert_eq!(fx.log(&fx.a), "", "nothing to log when nothing happens");
}

#[test]
fn fast_forward_when_behind() {
    let fx = Fixture::new();
    let up = upstream_commit(&fx, "new.txt", "hello\n");
    // Unrelated uncommitted work must survive the fast-forward.
    fs::write(fx.a.join("g.txt"), "local edit\n").unwrap();
    fs::write(fx.a.join("scratch.txt"), "untracked\n").unwrap();

    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Changed, "{r:?}");
    assert_eq!((r.pulled, r.rebased, r.pushed), (1, 0, 0));
    assert_eq!(head(&fx.a), up);
    assert_eq!(fs::read_to_string(fx.a.join("g.txt")).unwrap(), "local edit\n");
    assert!(fx.a.join("scratch.txt").exists());
    assert!(fx.log(&fx.a).contains("pulled 1 commit(s) -> "));
}

#[test]
fn pushes_local_commits_and_round_trips() {
    let fx = Fixture::new();
    let c = commit(&fx.a, "a.txt", "from a\n", "a commit");
    let r = fx.sync(&fx.a);
    assert_eq!((r.result, r.pushed), (RunResult::Changed, 1), "{r:?}");
    assert_eq!(fx.origin_head(), c);
    assert!(fx.log(&fx.a).contains("pushed 1 commit(s) -> "));

    let r = fx.sync(&fx.b);
    assert_eq!((r.result, r.pulled), (RunResult::Changed, 1), "{r:?}");
    assert_eq!(head(&fx.b), c);
}

#[test]
fn diverged_rebases_local_commits_then_pushes() {
    let fx = Fixture::new();
    upstream_commit(&fx, "up.txt", "upstream\n");
    let mine = commit(&fx.a, "mine.txt", "local\n", "local commit");
    // Dirty, non-overlapping work: autostash must put it back.
    fs::write(fx.a.join("g.txt"), "still editing\n").unwrap();
    fs::write(fx.a.join("notes.txt"), "untracked\n").unwrap();

    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Changed, "{r:?}");
    assert_eq!((r.pulled, r.rebased, r.pushed), (1, 1, 1));
    let after = r.after.clone().expect("state recorded after the run");
    assert_eq!((after.ahead, after.behind, after.dirty), (0, 0, 2));
    assert_eq!(after.head, Some(head(&fx.a)));
    assert!(after.head_matches_upstream());
    let h = head(&fx.a);
    assert_ne!(h, mine, "rebase rewrites the local commit id");
    assert_eq!(fx.origin_head(), h);
    assert_eq!(git(&fx.a, &["log", "--format=%s", "-3"]), "local commit\nupstream up.txt\ninit");
    assert_eq!(fs::read_to_string(fx.a.join("g.txt")).unwrap(), "still editing\n");
    assert!(fx.a.join("notes.txt").exists());
    assert_eq!(git(&fx.a, &["stash", "list"]), "", "autostash was applied and dropped");
    let log = fx.log(&fx.a);
    assert!(log.contains("rebased 1 local commit(s) onto 1 upstream -> "), "{log}");
    assert!(log.contains("pushed 1 commit(s) -> "), "{log}");
}

#[test]
fn overlap_with_uncommitted_change_skips_pull() {
    let fx = Fixture::new();
    upstream_commit(&fx, "f.txt", "one\nTWO\nthree\n");
    let before = head(&fx.a);
    fs::write(fx.a.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Skipped, "{r:?}");
    assert_eq!(r.reason, Some(SkipReason::Overlap));
    assert_eq!(r.overlap, vec!["f.txt".to_string()]);
    assert_eq!(head(&fx.a), before, "HEAD untouched");
    assert_eq!(
        fs::read_to_string(fx.a.join("f.txt")).unwrap(),
        "one\ntwo\nthree\nfour\n",
        "uncommitted work untouched"
    );
    assert!(
        fx.log(&fx.a)
            .contains("skip pull: uncommitted changes overlap upstream: f.txt")
    );

    // Logged once, not on every run.
    fx.sync(&fx.a);
    assert_eq!(fx.log(&fx.a).matches("skip pull").count(), 1);

    // Once the human resolves it, the next run pulls and says the skip is over.
    git(&fx.a, &["checkout", "--", "f.txt"]);
    let r = fx.sync(&fx.a);
    assert_eq!((r.result, r.pulled), (RunResult::Changed, 1), "{r:?}");
    assert!(fx.log(&fx.a).contains("ok again (was: skip pull"));
}

#[test]
fn overlap_with_untracked_file_in_new_directory_skips_pull() {
    // The script compared against `?? newdir/` and missed this; tandm lists untracked files.
    let fx = Fixture::new();
    upstream_commit(&fx, "newdir/file.txt", "upstream\n");
    let before = head(&fx.a);
    fs::create_dir_all(fx.a.join("newdir")).unwrap();
    fs::write(fx.a.join("newdir/file.txt"), "local, untracked\n").unwrap();

    let r = fx.sync(&fx.a);
    assert_eq!(r.reason, Some(SkipReason::Overlap), "{r:?}");
    assert_eq!(r.overlap, vec!["newdir/file.txt".to_string()]);
    assert_eq!(head(&fx.a), before);
    assert_eq!(
        fs::read_to_string(fx.a.join("newdir/file.txt")).unwrap(),
        "local, untracked\n"
    );
}

#[test]
fn rename_upstream_overlaps_with_edit_of_old_name() {
    let fx = Fixture::new();
    git(&fx.b, &["mv", "g.txt", "h.txt"]);
    git(&fx.b, &["commit", "-q", "-m", "rename"]);
    git(&fx.b, &["push", "-q", "origin", "main"]);
    fs::write(fx.a.join("g.txt"), "edited\n").unwrap();

    let r = fx.sync(&fx.a);
    assert_eq!(r.reason, Some(SkipReason::Overlap), "{r:?}");
    assert_eq!(r.overlap, vec!["g.txt".to_string()]);
}

#[test]
fn rebase_conflict_is_aborted_and_everything_left_as_it_was() {
    let fx = Fixture::new();
    upstream_commit(&fx, "f.txt", "one\nUPSTREAM\nthree\n");
    let mine = commit(&fx.a, "f.txt", "one\nLOCAL\nthree\n", "local edit of f");
    fs::write(fx.a.join("g.txt"), "dirty but unrelated\n").unwrap();
    let origin_before = fx.origin_head();

    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Skipped, "{r:?}");
    assert_eq!(r.reason, Some(SkipReason::RebaseConflict));
    assert!(r.is_failure(), "the script exited 1 here");
    assert_eq!(head(&fx.a), mine, "local commit untouched");
    assert_eq!(git(&fx.a, &["symbolic-ref", "--short", "HEAD"]), "main");
    let gd = PathBuf::from(git(&fx.a, &["rev-parse", "--absolute-git-dir"]));
    assert!(!gd.join("rebase-merge").exists() && !gd.join("rebase-apply").exists());
    assert_eq!(fs::read_to_string(fx.a.join("f.txt")).unwrap(), "one\nLOCAL\nthree\n");
    assert_eq!(fs::read_to_string(fx.a.join("g.txt")).unwrap(), "dirty but unrelated\n");
    assert_eq!(git(&fx.a, &["stash", "list"]), "", "autostash restored, not left behind");
    assert_eq!(fx.origin_head(), origin_before, "nothing pushed");
    assert!(fx.log(&fx.a).contains(
        "skip: rebasing 1 local commit(s) onto 1 upstream conflicts; resolve by hand"
    ));
}

#[test]
fn skips_while_a_git_operation_is_in_progress() {
    let fx = Fixture::new();
    upstream_commit(&fx, "up.txt", "x\n");
    let before = head(&fx.a);
    let gd = PathBuf::from(git(&fx.a, &["rev-parse", "--absolute-git-dir"]));
    for marker in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
    ] {
        let p = gd.join(marker);
        if marker.starts_with("rebase") {
            fs::create_dir(&p).unwrap();
        } else {
            fs::write(&p, "").unwrap();
        }
        let r = fx.sync(&fx.a);
        assert_eq!(r.result, RunResult::Skipped, "{marker}: {r:?}");
        assert_eq!(r.reason, Some(SkipReason::InProgress), "{marker}");
        assert!(r.message.contains(marker), "{marker}: {}", r.message);
        assert_eq!(head(&fx.a), before, "{marker}: HEAD untouched");
        if p.is_dir() {
            fs::remove_dir(&p).unwrap();
        } else {
            fs::remove_file(&p).unwrap();
        }
    }
    let r = fx.sync(&fx.a);
    assert_eq!((r.result, r.pulled), (RunResult::Changed, 1), "{r:?}");
}

#[test]
fn skips_when_not_on_the_branch() {
    let fx = Fixture::new();
    upstream_commit(&fx, "up.txt", "x\n");
    git(&fx.a, &["checkout", "-q", "-b", "feature"]);
    let c = commit(&fx.a, "feat.txt", "wip\n", "feature work");
    let origin_before = fx.origin_head();

    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Skipped, "{r:?}");
    assert_eq!(r.reason, Some(SkipReason::NotOnBranch));
    assert_eq!(r.message, "skip: not on main");
    assert_eq!(head(&fx.a), c);
    assert_eq!(fx.origin_head(), origin_before, "feature branch not pushed");
    assert!(
        git(&fx.a, &["rev-parse", "-q", "--verify", "origin/main"]) != origin_before,
        "and it doesn't even fetch"
    );

    // Detached HEAD is "not on main" too.
    git(&fx.a, &["checkout", "-q", "--detach"]);
    assert_eq!(fx.sync(&fx.a).reason, Some(SkipReason::NotOnBranch));
}

#[test]
fn configured_branch_other_than_main() {
    let fx = Fixture::new();
    git(&fx.a, &["checkout", "-q", "-b", "trunk"]);
    git(&fx.a, &["push", "-q", "origin", "trunk"]);
    let c = commit(&fx.a, "t.txt", "t\n", "on trunk");
    let mut cfg = fx.cfg(&fx.a);
    cfg.branch = "trunk".into();
    let r = sync_repo(&cfg, &fx.state("a")).unwrap().unwrap();
    assert_eq!((r.result, r.pushed), (RunResult::Changed, 1), "{r:?}");
    assert_eq!(git(&fx.origin, &["rev-parse", "trunk"]), c);
}

fn assert_secret_blocked(file: &str, content: &str, secret: &str) {
    let fx = Fixture::new();
    commit(&fx.a, file, content, "add config");
    let origin_before = fx.origin_head();

    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Skipped, "{file}: {r:?}");
    assert_eq!(r.reason, Some(SkipReason::Secrets), "{file}");
    assert!(r.is_failure());
    assert_eq!(r.secret_hits.len(), 1, "{:?}", r.secret_hits);
    assert_eq!(r.secret_hits[0].file, file);
    assert_eq!(fx.origin_head(), origin_before, "{file}: must not push");
    assert!(r.message.starts_with("NOT pushing 1 commit(s): secret-looking lines: "));
    // The secret itself never lands in the log or the status record.
    assert!(!fx.log(&fx.a).contains(secret), "secret leaked into log");
    assert!(!fx.json(&fx.a).contains(secret), "secret leaked into status json");
    assert!(!format!("{r:?}").contains(secret));
}

#[test]
fn secret_private_key_blocks_push() {
    assert_secret_blocked(
        "id_test",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAA",
    );
}

#[test]
fn secret_mongodb_uri_blocks_push() {
    assert_secret_blocked(
        "db.md",
        "connect with mongodb+srv://admin:hunter2pass@cluster0.example.net/app\n",
        "hunter2pass",
    );
}

#[test]
fn secret_credential_assignment_blocks_push() {
    assert_secret_blocked(
        "deploy.env",
        "FOO=bar\nGITEA_TOKEN=Zq8xR2vB7nM4kL1pW9sT6yU3hJ5gF0dA\n",
        "Zq8xR2vB7nM4kL1pW9sT6yU3hJ5gF0dA",
    );
}

#[test]
fn secret_added_then_removed_locally_still_blocks_push() {
    let fx = Fixture::new();
    commit(&fx.a, "x.env", "API_KEY=Zq8xR2vB7nM4kL1pW9sT6yU3hJ5gF0dA\n", "oops");
    commit(&fx.a, "x.env", "API_KEY=${API_KEY}\n", "remove it again");
    let r = fx.sync(&fx.a);
    assert_eq!(r.reason, Some(SkipReason::Secrets), "{r:?}");
    assert_eq!(r.secret_hits[0].file, "x.env");
    assert!(r.secret_hits[0].commit.is_some());
}

#[test]
fn placeholders_do_not_block_push() {
    let fx = Fixture::new();
    commit(
        &fx.a,
        "README.md",
        "MONGO_URI=mongodb://<user>:<password>@host/db\nAPI_KEY=${API_KEY}\nTOKEN=short\n",
        "docs",
    );
    let r = fx.sync(&fx.a);
    assert_eq!((r.result, r.pushed), (RunResult::Changed, 1), "{r:?}");
}

#[test]
fn second_run_while_one_holds_the_lock_is_a_no_op() {
    let fx = Fixture::new();
    upstream_commit(&fx, "up.txt", "x\n");
    let before = head(&fx.a);
    let files = StateFiles::new(&fx.state("a"), "r");
    let held = files.try_lock().unwrap().expect("first lock");
    assert!(sync_repo(&fx.cfg(&fx.a), &fx.state("a")).unwrap().is_none());
    assert_eq!(head(&fx.a), before);
    drop(held);
    assert!(sync_repo(&fx.cfg(&fx.a), &fx.state("a")).unwrap().is_some());
}

#[test]
fn paused_repo_is_left_alone() {
    let fx = Fixture::new();
    upstream_commit(&fx, "up.txt", "x\n");
    commit(&fx.a, "mine.txt", "y\n", "local");
    let before = head(&fx.a);
    let origin_before = fx.origin_head();
    let mut cfg = fx.cfg(&fx.a);
    cfg.paused = true;
    let r = sync_repo(&cfg, &fx.state("a")).unwrap().unwrap();
    assert_eq!(r.result, RunResult::Paused);
    assert_eq!(head(&fx.a), before);
    assert_eq!(fx.origin_head(), origin_before);
}

#[test]
fn fetch_failure_is_an_error_logged_once() {
    let fx = Fixture::new();
    git(&fx.a, &["remote", "set-url", "origin", "/nonexistent/tandm/origin.git"]);
    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Error, "{r:?}");
    assert_eq!(r.message, "fetch failed");
    assert!(r.detail.is_some(), "git's stderr is kept");
    fx.sync(&fx.a);
    fx.sync(&fx.a);
    assert_eq!(fx.log(&fx.a).matches("fetch failed").count(), 1);
}

#[test]
fn never_commits_or_stashes() {
    let fx = Fixture::new();
    upstream_commit(&fx, "up.txt", "x\n");
    commit(&fx.a, "mine.txt", "y\n", "local");
    fs::write(fx.a.join("g.txt"), "dirty\n").unwrap();
    fs::write(fx.a.join("new.txt"), "untracked\n").unwrap();
    let r = fx.sync(&fx.a);
    assert_eq!(r.result, RunResult::Changed, "{r:?}");
    // Exactly init + upstream + local; dirty work still dirty.
    assert_eq!(git(&fx.a, &["rev-list", "--count", "HEAD"]), "3");
    let st = git(&fx.a, &["status", "--porcelain"]);
    assert!(st.contains("M g.txt") && st.contains("?? new.txt"), "{st}");
    assert_eq!(git(&fx.a, &["stash", "list"]), "");
}
