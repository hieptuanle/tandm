//! `tandm`: keep git-synced repos in step across machines.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use tandm_core::config::{Config, MachineConfig};
use tandm_core::state::StateFiles;
use tandm_core::status::{self, CellState, MachineStatus, RepoStatus};
use tandm_core::{app, ops, paths, remote, service};

#[derive(Parser)]
#[command(name = "tandm", version, about = "Keep git-synced repos in step across machines")]
struct Cli {
    /// Run the command on a peer machine (from `tandm machine list`) over ssh
    #[arg(long, global = true, value_name = "MACHINE")]
    on: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start syncing a repo on this machine
    Add {
        path: String,
        /// Row name shared across machines (default: directory name)
        #[arg(long)]
        name: Option<String>,
        /// Branch to keep in sync (default: main)
        #[arg(long)]
        branch: Option<String>,
        /// Remote to sync with (default: origin)
        #[arg(long)]
        remote: Option<String>,
    },
    /// Stop syncing a repo on this machine (the checkout itself is not touched)
    Rm { repo: String },
    /// Repos on this machine
    List {
        #[arg(long)]
        json: bool,
    },
    /// Sync state of this machine's repos (and, with --all, every peer's over ssh)
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        all: bool,
    },
    /// Pull and push now: one repo, or all unpaused repos (what the service runs)
    Sync {
        repo: Option<String>,
        /// Print nothing unless something fails
        #[arg(long, short)]
        quiet: bool,
    },
    /// Stop syncing a repo until `resume`
    Pause { repo: String },
    /// Resume syncing a paused repo
    Resume { repo: String },
    /// Show a repo's log (actions and changes of state only)
    Log {
        repo: String,
        #[arg(short = 'n', default_value_t = 30)]
        lines: usize,
        #[arg(long)]
        json: bool,
    },
    /// Peer machines shown by `status --all` and the GUI
    #[command(subcommand)]
    Machine(MachineCmd),
    /// Background job that runs `tandm sync` every interval (systemd user timer / launchd agent)
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Open the grid window
    Gui,
    /// Launcher entry for the window (macOS ~/Applications/tandm.app, Linux .desktop file)
    #[command(subcommand)]
    App(AppCmd),
}

#[derive(Subcommand)]
enum AppCmd {
    /// Create/refresh it, pointing at this tandm binary
    Install,
    /// Remove it
    Uninstall,
}

#[derive(Subcommand)]
enum MachineCmd {
    /// Add a peer reachable by ssh
    Add {
        name: String,
        /// ssh destination (default: the name, e.g. an ~/.ssh/config alias)
        #[arg(long)]
        ssh: Option<String>,
        /// How to run tandm there (default: `tandm`), e.g. ~/.local/bin/tandm
        #[arg(long)]
        tandm: Option<String>,
    },
    /// Remove a peer (its own tandm setup is untouched)
    Rm { name: String },
    /// This machine and its peers
    List,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Write and start the timer/agent (re-run after changing interval_secs)
    Install,
    /// Stop and remove the timer/agent
    Uninstall,
    /// Whether the timer/agent is installed and running
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cmd = match cli.cmd {
        Some(c) => c,
        // Opened as tandm.app from Finder/Dock/Spotlight: no arguments, show the window.
        None if app::launched_from_bundle() => Cmd::Gui,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            return ExitCode::from(2);
        }
    };
    let result = match cli.on {
        Some(m) => run_on(&m, cmd),
        None => run(cmd),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tandm: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd) -> Result<ExitCode> {
    let state_dir = paths::state_dir();
    match cmd {
        Cmd::Add {
            path,
            name,
            branch,
            remote,
        } => {
            let mut cfg = Config::load()?;
            let added = ops::add_repo(
                &mut cfg,
                &path,
                name.as_deref(),
                branch.as_deref(),
                remote.as_deref(),
            )?;
            cfg.save()?;
            StateFiles::new(&state_dir, &added.repo.name)
                .append_log("added to tandm")
                .ok();
            println!(
                "added {} ({}, {} ↔ {})",
                added.repo.name,
                added.repo.path.display(),
                added.repo.branch,
                added.repo.upstream()
            );
            for w in added.warnings {
                println!("note: {w}");
            }
            if !service::state().active {
                println!("note: the service isn't running here; `tandm service install` to sync in the background");
            }
        }
        Cmd::Rm { repo } => {
            let mut cfg = Config::load()?;
            let r = ops::remove_repo(&mut cfg, &repo, &state_dir)?;
            cfg.save()?;
            println!("removed {} ({}); the checkout is untouched", r.name, r.path.display());
        }
        Cmd::List { json } => {
            let cfg = Config::load()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&cfg.repos)?);
            } else if cfg.repos.is_empty() {
                println!("no repos yet; `tandm add <path>`");
            } else {
                let w = cfg.repos.iter().map(|r| r.name.len()).max().unwrap_or(0);
                for r in &cfg.repos {
                    println!(
                        "{:w$}  {}  {}{}",
                        r.name,
                        paths::contract_tilde(&r.path.to_string_lossy()),
                        r.upstream(),
                        if r.paused { "  (paused)" } else { "" }
                    );
                }
            }
        }
        Cmd::Status { json, all } => {
            let cfg = Config::load()?;
            let local = status::local_status(&cfg, &state_dir);
            let peers = if all {
                remote::all_status(&cfg.machines)
            } else {
                Vec::new()
            };
            if json {
                if all {
                    let v: Vec<serde_json::Value> = std::iter::once(serde_json::to_value(&local)?)
                        .chain(peers.into_iter().map(|(m, r)| match r {
                            Ok(s) => serde_json::to_value(s).unwrap_or_default(),
                            Err(e) => serde_json::json!({
                                "machine": m.name, "unreachable": format!("{e:#}")
                            }),
                        }))
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&local)?);
                }
            } else {
                print_machine(&local, true);
                for (m, r) in peers {
                    println!();
                    match r {
                        Ok(s) => print_machine(&s, false),
                        Err(e) => println!("{}: unreachable — {e:#}", m.name),
                    }
                }
            }
        }
        Cmd::Sync { repo, quiet } => {
            let cfg = Config::load()?;
            if cfg.repos.is_empty() && !quiet {
                println!("no repos yet; `tandm add <path>`");
            }
            let mut failed = false;
            for (name, r) in ops::sync(&cfg, repo.as_deref(), &state_dir)? {
                match r {
                    Ok(Some(rec)) => {
                        failed |= rec.is_failure();
                        if rec.is_failure() {
                            eprintln!("{name}: {}", rec.message);
                        } else if !quiet {
                            println!("{name}: {}", rec.message);
                        }
                    }
                    Ok(None) => {
                        if !quiet {
                            println!("{name}: another sync is running; skipped");
                        }
                    }
                    Err(e) => {
                        failed = true;
                        eprintln!("{name}: {e:#}");
                    }
                }
            }
            if failed {
                return Ok(ExitCode::FAILURE);
            }
        }
        Cmd::Pause { repo } => set_paused(&repo, true, &state_dir)?,
        Cmd::Resume { repo } => set_paused(&repo, false, &state_dir)?,
        Cmd::Log { repo, lines, json } => {
            let cfg = Config::load()?;
            let r = cfg.repo(&repo)?;
            let tail = StateFiles::new(&state_dir, &r.name).log_tail(lines);
            if json {
                println!("{}", serde_json::to_string(&tail)?);
            } else {
                for l in tail {
                    println!("{l}");
                }
            }
        }
        Cmd::Machine(m) => machine(m)?,
        Cmd::Service(s) => match s {
            ServiceCmd::Install => {
                let cfg = Config::load()?;
                let exe = std::env::current_exe().context("finding tandm's own path")?;
                println!("{}", service::install(&exe, cfg.interval_secs)?);
            }
            ServiceCmd::Uninstall => println!("{}", service::uninstall()?),
            ServiceCmd::Status => {
                let s = service::state();
                println!(
                    "{} ({}installed, {})",
                    s.detail,
                    if s.installed { "" } else { "not " },
                    if s.active { "active" } else { "inactive" }
                );
            }
        },
        Cmd::Gui => gui()?,
        Cmd::App(a) => match a {
            AppCmd::Install => {
                let exe = std::env::current_exe().context("finding tandm's own path")?;
                println!("{}", app::install(&exe)?);
            }
            AppCmd::Uninstall => println!("{}", app::uninstall()?),
        },
    }
    Ok(ExitCode::SUCCESS)
}

/// `--on <machine>`: forward the same command line to that peer over ssh, streaming its output
/// and exit code. The peer must be configured with `tandm machine add`.
fn run_on(machine: &str, cmd: Cmd) -> Result<ExitCode> {
    if matches!(cmd, Cmd::Gui | Cmd::App(_)) {
        bail!("`gui`/`app` can't run on another machine; the window already shows every peer");
    }
    let cfg = Config::load()?;
    if machine == cfg.machine_name() {
        // Same as not passing --on.
        return run(cmd);
    }
    let Some(m) = cfg.machines.iter().find(|m| m.name == machine) else {
        bail!("no machine {machine:?} (see `tandm machine list`)");
    };
    // Re-use the user's own arguments minus `--on X` / `--on=X`.
    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--on" {
            it.next();
        } else if !a.starts_with("--on=") {
            args.push(a);
        }
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let status = remote::ssh_command(m, &argv)
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("running ssh {}", m.ssh_target()))?;
    Ok(match status.code() {
        Some(0) => ExitCode::SUCCESS,
        Some(255) => {
            eprintln!("tandm: {} unreachable over ssh ({})", m.name, m.ssh_target());
            ExitCode::FAILURE
        }
        Some(c) => ExitCode::from(c.clamp(1, 254) as u8),
        None => ExitCode::FAILURE,
    })
}

fn set_paused(repo: &str, paused: bool, state_dir: &std::path::Path) -> Result<()> {
    let mut cfg = Config::load()?;
    let changed = ops::set_paused(&mut cfg, repo, paused, state_dir)?;
    cfg.save()?;
    let verb = if paused { "paused" } else { "resumed" };
    if changed {
        println!("{verb} {repo}");
    } else {
        println!("{repo} was already {verb}");
    }
    Ok(())
}

fn machine(cmd: MachineCmd) -> Result<()> {
    let mut cfg = Config::load()?;
    match cmd {
        MachineCmd::Add { name, ssh, tandm } => {
            if cfg.machines.iter().any(|m| m.name == name) {
                bail!("machine {name:?} is already configured");
            }
            if name == cfg.machine_name() {
                bail!("{name:?} is this machine; peers only");
            }
            let m = MachineConfig { name, ssh, tandm };
            let probe = remote::status(&m);
            cfg.machines.push(m.clone());
            cfg.save()?;
            match probe {
                Ok(s) => println!(
                    "added {} (answers as {}, tandm {}, {} repo(s))",
                    m.name,
                    s.machine,
                    s.tandm_version,
                    s.repos.len()
                ),
                Err(e) => println!("added {}, but it doesn't answer yet: {e:#}", m.name),
            }
        }
        MachineCmd::Rm { name } => {
            let before = cfg.machines.len();
            cfg.machines.retain(|m| m.name != name);
            if cfg.machines.len() == before {
                bail!("no machine {name:?}");
            }
            cfg.save()?;
            println!("removed {name}");
        }
        MachineCmd::List => {
            println!("{} (this machine)", cfg.machine_name());
            for m in &cfg.machines {
                println!("{}  ssh {}  {}", m.name, m.ssh_target(), m.tandm_cmd());
            }
        }
    }
    Ok(())
}

fn print_machine(s: &MachineStatus, this: bool) {
    println!(
        "{}{} — {} · tandm {} · {}",
        s.machine,
        if this { " (this machine)" } else { "" },
        s.os,
        s.tandm_version,
        if s.service.active {
            "service active".to_string()
        } else {
            format!("SERVICE NOT RUNNING ({})", s.service.detail)
        }
    );
    if s.repos.is_empty() {
        println!("  (no repos)");
    }
    let w = s.repos.iter().map(|r| r.name.len()).max().unwrap_or(0);
    for r in &s.repos {
        println!("  {:w$}  {}", r.name, repo_line(r, s.now));
    }
}

fn repo_line(r: &RepoStatus, now: i64) -> String {
    let state = CellState::of(r);
    let mut parts = vec![format!("{:<14}", state.label())];
    if let Some(l) = &r.live {
        parts.push(format!(
            "{} {} {}/{}",
            status::short_sha(&l.head),
            if l.head_matches_upstream() { "=" } else { "≠" },
            r.remote,
            r.branch
        ));
        if l.dirty > 0 {
            parts.push(format!("dirty {}", l.dirty));
        }
    }
    if let Some(run) = &r.last_run {
        parts.push(format!("last run {}: {}", status::ago(run.at, now), run.message));
    }
    if let CellState::Error(e) = &state {
        if r.last_run.as_ref().map(|x| &x.message) != Some(e) {
            parts.push(e.clone());
        }
    }
    parts.join("  ")
}

#[cfg(feature = "gui")]
fn gui() -> Result<()> {
    tandm_gui::run()
}

#[cfg(not(feature = "gui"))]
fn gui() -> Result<()> {
    bail!("this tandm was built without the GUI (cargo feature `gui`)")
}
