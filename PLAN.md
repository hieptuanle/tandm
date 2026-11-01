# tandm — plan

Design notes: the crate layout, the GPUI pin, how the GUI gets remote status, and every place the
engine deliberately differs from the bash script it replaces (`git-autosync.sh`, a timer-driven script
that pulled and pushed one checkout every 2 minutes and never auto-committed; tandm ports its
behaviour and adds a multi-repo, multi-machine view).

## Crate layout (Cargo workspace)

```
crates/
  tandm-core/   lib, no GUI deps: config, state files, git wrapper, sync engine, status model,
                remote status over ssh, service unit writers. All tests live here.
  tandm-gui/    lib: the GPUI window (grid + detail panel + actions). Depends on tandm-core.
  tandm/        bin `tandm`: clap CLI. Feature `gui` (on by default) pulls in tandm-gui.
                `--no-default-features` builds a headless binary (fast, no GPU deps).
```

Splitting core out lets `cargo test -p tandm-core` run without compiling GPUI, which matters on a
small machine where builds have to run throttled (`nice -n 15 cargo … -j 2`).

Dependencies stay few: `clap`, `serde`/`serde_json`, `toml`, `regex`, `anyhow`, `gpui`. Git is the
`git` CLI, not libgit2/gitoxide. Shelling out matches the script's behaviour exactly (rebase
autostash, ssh config, hooks, credential helpers) and uses whatever git the user already trusts.
File locking uses `std::fs::File::try_lock` (stable since Rust 1.89), so no extra crate.

## GPUI version pin

`gpui = "=0.2.2"` from crates.io, the last published release (Oct 2025).

- Zed's `main` has moved on (gpui split into `gpui_platform`/`gpui_wgpu`/…) but is unpublished and
  needs Rust 1.98.1, newer than the toolchains tandm targets (1.89+).
  A git dependency on the Zed monorepo would also be a very large fetch. So the crates.io pin it is.
- macOS: Metal renderer. Xcode 26 doesn't ship the Metal toolchain by default (it's a separate
  download), so tandm-gui enables gpui's `runtime_shaders` feature on macOS. That compiles the
  shaders at startup, and a plain Xcode install is enough to build.
- Linux: gpui 0.2.2 renders through blade (Vulkan) on Wayland/X11, so the GUI needs a Vulkan driver
  (an ICD, not just `vulkan-icd-loader`). Without one it exits with a clear message. The CLI and engine
  don't need a GPU.
- gpui 0.2.2's own `Cargo.lock` is the starting point for ours: newer libc releases break its
  `xattr` dependency on Linux.

## Files on disk (each machine)

| What | Linux / macOS | Windows (later) |
|---|---|---|
| config | `$XDG_CONFIG_HOME/tandm/config.toml` (default `~/.config/tandm/`) | `%APPDATA%\tandm\` |
| state | `$XDG_STATE_HOME/tandm/` (default `~/.local/state/tandm/`) | `%LOCALAPPDATA%\tandm\` |

Per repo in the state dir: `<name>.json` (last run record, written atomically via tmp + rename),
`<name>.log` (actions and state changes only), `<name>.lock` (run lock).

```toml
# config.toml
interval_secs = 120            # used by `service install`

[[repo]]
name = "notes"                 # row key across machines; defaults to the directory name
path = "/home/alice/projects/notes"
branch = "main"                # default "main"
remote = "origin"              # default "origin"
paused = false

[[machine]]                    # peers the GUI/`status --all` shows besides this machine
name = "laptop-b"
ssh = "laptop-b"               # ssh alias / host
tandm = "tandm"                # remote command; e.g. "~/.local/bin/tandm"
```

Repos are matched across machines (grid rows) by `name`, since the same repo lives at different
paths on Linux and macOS. `tandm add` refuses a name that is already taken; `--name` overrides it.

## Sync engine (port of the script)

Same steps and order as the script, per repo:

1. Take a non-blocking `flock` on `<name>.lock`. If another run holds it, exit quietly. The kernel
   drops the lock when a process dies, so the script's 10-minute stale-lock cleanup isn't needed.
2. Set `GIT_SSH_COMMAND="ssh -o BatchMode=yes -o ConnectTimeout=15"` and `GIT_TERMINAL_PROMPT=0`
   for every git child.
3. Skip if not on the configured branch, or if `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
   `rebase-merge`, `rebase-apply` or `index.lock` exists in the git dir.
4. `git fetch -q <remote> <branch>`; on failure → error "fetch failed".
5. Compute ahead/behind against `<remote>/<branch>`.
6. If behind: overlap = (files changed `merge-base..upstream`) ∩ (uncommitted + untracked files). If
   any overlap, skip and record the list. Else, if ahead == 0, `merge --ff-only`; otherwise
   `-c rebase.autoStash=true rebase`. If the rebase fails, `rebase --abort` and skip with the reason.
7. If ahead > 0 after that: scan `git diff <upstream> HEAD` added lines with the script's three
   regexes. On a hit → don't push, record why. Else `git push -q <remote> <branch>`; on failure →
   error "push failed".
8. Record outcome + live counts. Never commits, never stashes by hand, never force-pushes.

Places where it deliberately differs from the script, all in the safer or quieter direction:

- **`--no-optional-locks`** on read-only git commands (`status`, `diff`, `rev-list`). Otherwise
  `git status` can briefly take `index.lock` to refresh stat info, which can make a parallel agent's
  `git commit` fail.
- **Overlap uses `status --porcelain -z --untracked-files=all`**. With the script's default, an
  untracked *directory* shows as `dir/`, which never matches `dir/file` from upstream, so the pull
  went ahead and then failed. `-z` also avoids path-quoting issues. Both sides of a rename count.
- **Upstream file list uses `--no-renames`**, so a file that upstream renamed counts under its old
  name too. Without that, editing `a.txt` locally while upstream renames `a.txt → b.txt` wasn't seen
  as an overlap.
- **Secrets are scanned per outgoing commit** (`git log -p <upstream>..HEAD`) instead of the net
  `git diff <upstream> HEAD`. A secret committed and then removed in a later local commit would
  otherwise be pushed in history. Same three regexes.
- **Secret block doesn't copy the secret into the log**. The script logged the matching line
  (truncated to 120 chars), value included. tandm records `file:line (rule)` only, because the status
  JSON travels to other machines and shows up in the GUI.
- **Errors are logged once per state change**, along with the first lines of git's stderr. The script
  appended fetch stderr to the log on every failed run, i.e. every 2 min while offline. When a
  skip or error clears, tandm logs `ok again (was: …)`, which is also a change of state.
- **Autostash conflict detection**: if the rebase succeeds but git reports that the autostash
  couldn't be re-applied, tandm records an error pointing at `git stash list`. The overlap check
  should make this impossible, short of a race with an editor writing mid-rebase.
- Branch and remote are per-repo config instead of hard-coded `main` / `origin`.
- **Paused repos** are left alone by the service, by `tandm sync`, and by `tandm sync <repo>`
  (it says "paused"); `resume` first. Live status is still reported.

Outcome model (`status --json`, stable field names):

```
outcome: synced | changed{pulled, rebased, pushed} | skipped{reason, detail} | error{message} | paused
skip reasons: not_on_branch, in_progress(op), overlap(files), rebase_conflict, secrets(hits)
```

Each record also carries `last_run`, `last_change`, `message` (human text, same wording as the
script), and a `live` block computed at `status` time with no network: branch, HEAD,
`<remote>/<branch>` (local ref from the last fetch), ahead, behind, dirty count, in-progress op,
`head_matches_upstream`. So a commit made 30 s ago already shows as "ahead 1" before the next run.

## How the GUI learns other machines' state: `tandm status --json` over SSH

Each GUI polls every peer in `[[machine]]` with

```
ssh -o BatchMode=yes -o ConnectTimeout=5 <alias> <tandm> status --json
```

on background threads every 30 s, plus on demand. Local status is read in-process.

- No daemon, no new port. Machines reach each other by ssh alias (e.g. over a VPN), and ssh
  handles auth. A peer that doesn't answer shows as **unreachable** in its whole column, with the ssh
  error on hover/click.
- The status JSON includes `machine` (hostname), `tandm_version`, `now` (for clock skew), and service
  state (installed/active). The GUI can then flag a machine whose timer isn't running.
- Remote actions go over the same channel: `ssh <alias> tandm sync <name>`, `pause`, `resume`, `add`,
  `rm`, `log <name> -n 50 --json`. Every argument is single-quoted for the remote shell.
- `tandm status --all [--json]` does the same fan-out in the CLI, which is handy for checking from a
  terminal or a script.

## CLI

```
tandm add <path> [--name N] [--branch main] [--remote origin]
tandm rm <name|path>
tandm list
tandm status [--json] [--all]        # --all includes peers over ssh
tandm sync [repo]                    # all unpaused repos when omitted; what the service runs
tandm pause <repo> | resume <repo>
tandm log <repo> [-n 50] [--json]
tandm machine add <name> [--ssh alias] [--tandm cmd] | machine rm <name> | machine list
tandm service install | uninstall | status
tandm gui
```

`service install` writes one unit that runs `tandm sync` for all repos every `interval_secs`:

- Linux: `~/.config/systemd/user/tandm.{service,timer}` (oneshot; `OnBootSec=1min`,
  `OnUnitActiveSec=2min`), then `systemctl --user daemon-reload && enable --now tandm.timer`.
- macOS: `~/Library/LaunchAgents/local.tandm.sync.plist` (`StartInterval`, `RunAtLoad`, a PATH that
  includes Homebrew and the binary's dir), then `launchctl bootstrap gui/<uid>`.
- Windows: not implemented yet (would be Task Scheduler); returns an error saying so.

Repos sync in parallel threads, each under its own lock, so one slow fetch doesn't hold up the rest.

## GUI (tandm-gui)

- Grid: header row of machines (this machine first, then peers), one row per repo name (union over
  machines). Each cell shows a coloured state pill (in sync / ahead N / behind N / diverged / dirty N /
  skipped: reason / error / unreachable / not added / paused), the last sync as relative time, the
  short HEAD, and ✓ or ≠ for "HEAD == origin/main".
- Clicking a cell opens a detail panel with path, branch, full message, overlap files, secret hits
  (file:line only), log tail, and the actions **Sync now**, **Pause/Resume**, **Remove**.
- An empty cell (repo not on that machine) offers **Add here**, with the path pre-filled from another
  machine's path (home dir swapped), editable.
- Toolbar: Refresh, Add repo… (machine + path). A minimal single-line text input (typing, backspace,
  paste) is enough for paths. No extra widget library.
- Cell state priority: unreachable > error > skipped > paused > diverged > behind > ahead > in sync.
  Dirty shows as a secondary badge, because uncommitted work is normal in shared worktrees.

## Tests (tandm-core, real git in temp dirs)

A fixture builds a bare `origin` plus clones `a` and `b` (with a local `user.name`/`email`, so no global
config is needed), and the state dir is pointed into the temp dir. Cases:

ff pull · diverged → rebase then push · overlap skip (tracked and untracked-in-new-dir) ·
rebase conflict → aborted, tree/HEAD/stash untouched · in-progress skip (each marker incl. index.lock)
· secret block (each of the 3 rules, and the secret value never appears in the log/status) ·
not-on-branch skip · lock held → no-op · paused → untouched · push of local commits.

## Switching a repo over from another sync tool

Never run two sync tools against the same checkout at once. On each machine, one at a time:

1. Install `tandm` in the same place on every machine (`~/.local/bin`). That dir isn't on the
   non-interactive ssh `PATH` of every Linux distro, so a peer may need `tandm = "~/.local/bin/tandm"`.
2. Stop the old timer **without removing it**, and wait until no old run is in flight. It can be
   re-enabled if anything goes wrong.
3. `tandm add <path>`, `tandm machine add <peer>`, `tandm sync`, then `tandm service install`.

Once every machine runs tandm, check push auth without changing the remote (`git push --dry-run`),
then watch the logs until a commit made on each machine shows "pushed" there and "pulled" on the
others. Only then remove the old tool.
