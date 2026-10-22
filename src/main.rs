mod cargo;
mod config;
mod daemon;
mod discovery;
mod env;
mod executor;
#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "linux")]
mod inotify;
mod inventory;
#[cfg(target_os = "macos")]
mod launchd;
mod planner;
mod sys;
#[cfg(target_os = "linux")]
mod systemd;

#[cfg(target_os = "macos")]
use launchd as service;
#[cfg(target_os = "linux")]
use systemd as service;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use config::Config;
use daemon::Cleaner;
use discovery::State;
use env::Env;
use executor::{Executor, format_bytes};
use inventory::{Item, Usage, Workspace};
use sys::DiskSpace;

/// Deletes the Bazel caches and Cargo build output you no longer use, before they fill your disk.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file [default: ~/.config/cendrillon/config.toml]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the Bazel caches and Cargo targets found, with their size and last use.
    Status,
    /// Run one full cleaning pass now.
    Clean {
        /// Report what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete the build units of one Cargo target that nothing current depends on.
    Trim {
        /// The target directory, usually `<workspace>/target`.
        target: PathBuf,
        /// Report what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Walk the whole home directory for caches and targets now, rather than follow its changes.
    Scan,
    /// Clean continuously in the foreground; this is what launchd or systemd runs.
    Daemon,
    /// Start the daemon now and at every login, as a launchd agent or a systemd user service.
    Install,
    /// Stop the daemon and remove the launchd agent or systemd user service.
    Uninstall,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let env = Env::current()?;
    let config = Config::load(&cli.config.unwrap_or_else(|| env.config_file()))?;
    match cli.command {
        Command::Status => status(&env, &config),
        Command::Clean { dry_run } => {
            let stop = AtomicBool::new(false);
            let cleaner = Cleaner {
                env: &env,
                config: &config,
                executor: executor(&config, dry_run, &stop),
            };
            let freed = cleaner.full_pass(&State::load(&env.state_file()))?;
            report(cleaner.executor.dry_run, freed);
            Ok(())
        }
        Command::Trim { target, dry_run } => {
            let root = target
                .canonicalize()
                .with_context(|| format!("reading {}", target.display()))?;
            let target = cargo::Target::classify(&root)
                .with_context(|| format!("{} is not a Cargo target directory", root.display()))?;
            let stop = AtomicBool::new(false);
            let executor = executor(&config, dry_run, &stop);
            let freed = executor.run(&[planner::Action::TrimTarget(target)]);
            report(executor.dry_run, freed);
            Ok(())
        }
        Command::Scan => daemon::refresh(
            &env,
            &config,
            &mut State::load(&env.state_file()),
            discovery::HomeSearch::Walk,
        ),
        Command::Daemon => daemon::run(&env, &config),
        Command::Install => service::install(&env),
        Command::Uninstall => service::uninstall(&env),
    }
}

fn executor<'a>(config: &Config, dry_run: bool, stop: &'a AtomicBool) -> Executor<'a> {
    Executor {
        dry_run: dry_run || config.dry_run,
        stop,
        unit_idle: config.unit_idle,
        observed: &no_observed_reads,
    }
}

/// Only the daemon watches reads as they happen.
fn no_observed_reads(_: &std::path::Path) -> cargo::Observed {
    cargo::Observed::new()
}

fn report(dry_run: bool, freed: u64) {
    let verb = if dry_run { "Would free" } else { "Freed" };
    println!("{verb} {}.", format_bytes(freed));
}

fn status(env: &Env, config: &Config) -> Result<()> {
    let inventory = discovery::discover(env, &config.extra_roots, &State::load(&env.state_file()));
    let usage = Usage::measure(inventory.items(), config.unit_idle);
    let mut rows: Vec<(u64, Item)> = inventory
        .items()
        .map(|item| (usage.size(item.path()), item))
        .collect();
    rows.sort_by_key(|(size, _)| std::cmp::Reverse(*size));

    println!("{:>9}  {:>9}  {:<16}  PATH", "SIZE", "LAST USED", "KIND");
    for (size, item) in &rows {
        let (kind, last_used, note) = describe(item, &usage);
        let last_used = last_used.map_or("-".into(), ago);
        let note = note.map(|note| format!("  ({note})")).unwrap_or_default();
        println!(
            "{:>9}  {last_used:>9}  {kind:<16}  {}{note}",
            format_bytes(*size),
            item.path().display()
        );
    }
    let total: u64 = rows.iter().map(|(size, _)| size).sum();
    let disk = DiskSpace::of(&env.home)?;
    println!(
        "\n{} in Bazel caches and Cargo targets. {} free of {} ({:.0}%).",
        format_bytes(total),
        format_bytes(disk.available),
        format_bytes(disk.total),
        disk.free_percent()
    );
    Ok(())
}

fn describe(item: &Item, usage: &Usage) -> (&'static str, Option<SystemTime>, Option<String>) {
    match item {
        Item::OutputBase(base) => {
            let workspace = match &base.workspace {
                Workspace::Present(path) => format!("workspace {}", path.display()),
                Workspace::Missing(path) => format!("workspace deleted: {}", path.display()),
                Workspace::Unreachable(path) => {
                    format!("workspace unreachable: {}", path.display())
                }
            };
            let server = sys::bazel_server(&base.path)
                .map(|pid| format!(", server pid {pid}"))
                .unwrap_or_default();
            (
                "output base",
                Some(base.last_used),
                Some(workspace + &server),
            )
        }
        Item::InstallBase(base) => ("install base", Some(base.last_used), None),
        Item::Cache(cache) => match cache.kind {
            inventory::CacheKind::Disk => ("disk cache", None, None),
            inventory::CacheKind::Repository => ("repository cache", None, None),
        },
        Item::CargoTarget(target) => (
            "cargo target",
            Some(target.last_used),
            Some(format!(
                "{} superseded",
                format_bytes(usage.garbage(&target.root))
            )),
        ),
        Item::Trash(_) => ("trash", None, Some("deletion pending".into())),
    }
}

fn ago(time: SystemTime) -> String {
    let secs = time.elapsed().unwrap_or_default().as_secs();
    match secs {
        0..3600 => format!("{}m ago", secs / 60),
        3600..172_800 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
