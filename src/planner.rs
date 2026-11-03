//! The cleaning policy: what to delete, given what is on disk and how much space is needed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cargo::Target;
use crate::config::{Config, WorktreeConfig};
use crate::inventory::{AGE_BUCKET, Cache, InstallBase, Inventory, OutputBase, Usage, Workspace};
use crate::worktree::{self, Blocker, Checks, Done, Prunable, Worktree};

#[derive(Debug)]
pub enum Sweep {
    /// Frequent and cheap: finish interrupted deletions and drop output bases of deleted
    /// workspaces.
    Orphans,
    /// Also apply the idle deadlines, and free space when the disk is nearly full.
    Full { pressure: Option<Pressure> },
}

/// The disk is nearly full: `bytes_to_free` must go, least recently used first.
#[derive(Debug)]
pub struct Pressure {
    pub bytes_to_free: u64,
    /// Measurements of the items on the full disk. Only these are evicted for pressure: deleting
    /// anything elsewhere would not help.
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    PurgeTrash(PathBuf),
    /// Skipped if Bazel used the output base again after `base.last_used`.
    DeleteOutputBase {
        base: OutputBase,
        reason: Reason,
    },
    /// Skipped if Bazel used the install base again after `base.last_used`.
    DeleteInstallBase {
        base: InstallBase,
        reason: Reason,
    },
    /// Evict the entries of `cache` last used before `older_than`.
    EvictEntries {
        cache: Cache,
        older_than: SystemTime,
        reason: Reason,
    },
    /// Delete the build units of a Cargo target that nothing current depends on.
    TrimTarget(Target),
    /// Skipped if Cargo used the target again after `target.last_used`.
    DeleteTarget {
        target: Target,
        reason: Reason,
    },
    /// Remove a linked git worktree, after checking everything again.
    ReapWorktree {
        worktree: Box<Worktree>,
        reason: Reason,
    },
    /// Drop git's record of a worktree whose directory is gone.
    PruneWorktree(Prunable),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    WorkspaceDeleted(PathBuf),
    Idle(Duration),
    DiskPressure,
    /// A worktree whose branch is finished, idle this long.
    Finished(Done, Duration),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::WorkspaceDeleted(path) => write!(f, "workspace {} is gone", path.display()),
            Reason::Idle(idle) => write!(f, "unused for {}", in_minutes(*idle)),
            Reason::DiskPressure => f.write_str("disk is nearly full"),
            Reason::Finished(done, idle) => write!(f, "{done}, unused for {}", in_minutes(*idle)),
        }
    }
}

fn in_minutes(duration: Duration) -> humantime::FormattedDuration {
    humantime::format_duration(Duration::from_secs(duration.as_secs() / 60 * 60))
}

/// What becomes of a linked worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Reap(Reason),
    Keep(Keep),
}

/// Why a worktree stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    Excluded,
    /// Locked with `git worktree lock`, for this reason.
    Locked(String),
    /// Last active this long ago, within its deadline.
    Active(Duration),
    /// Its branch is still on this remote branch, unmerged, and it was last active this long
    /// ago: past the usual deadline, within the longer one of pushed branches.
    Pushed(String, Duration),
    Blocked(Blocker),
    /// Removable as far as the cheap facts go; the slower checks did not run.
    Unchecked,
}

impl Keep {
    /// A few words, to count worktrees by why they stay.
    pub fn label(&self) -> &'static str {
        match self {
            Keep::Excluded => "excluded",
            Keep::Locked(_) => "locked",
            Keep::Active(_) => "active",
            Keep::Pushed(..) => "pushed, not merged",
            Keep::Blocked(blocker) => blocker.label(),
            Keep::Unchecked => "not checked",
        }
    }
}

impl fmt::Display for Keep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Keep::Excluded => f.write_str("excluded in the config"),
            Keep::Locked(why) if why.is_empty() => f.write_str("locked"),
            Keep::Locked(why) => write!(f, "locked: {why}"),
            Keep::Active(ago) => write!(f, "active {} ago", coarse(*ago)),
            Keep::Pushed(remote, ago) => {
                write!(f, "on {remote}, not merged; active {} ago", coarse(*ago))
            }
            Keep::Blocked(blocker) => blocker.fmt(f),
            Keep::Unchecked => f.write_str("not checked yet"),
        }
    }
}

/// `3d`, `5h` or `12m`.
pub fn coarse(duration: Duration) -> String {
    match duration.as_secs() {
        secs @ 0..3600 => format!("{}m", secs / 60),
        secs @ 3600..172_800 => format!("{}h", secs / 3600),
        secs => format!("{}d", secs / 86_400),
    }
}

/// Whether a worktree goes. Every condition must hold: not excluded, not locked, idle past
/// its deadline (shorter once its branch is finished, longer while it is pushed and not
/// finished), and the slower checks passed: clean,
/// no operation under way, no protected ignored file, nothing using it.
pub fn verdict(worktree: &Worktree, now: SystemTime, config: &WorktreeConfig) -> Verdict {
    if worktree::is_excluded(&worktree.path, &config.exclude) {
        return Verdict::Keep(Keep::Excluded);
    }
    if let Some(why) = &worktree.locked {
        return Verdict::Keep(Keep::Locked(why.clone()));
    }
    let idle = now
        .duration_since(worktree.last_activity)
        .unwrap_or_default();
    let deadline = match (&worktree.done, &worktree.pushed) {
        (Some(_), _) => config.idle_when_merged,
        (None, Some(_)) => config.idle_when_pushed,
        (None, None) => config.idle,
    };
    if idle < deadline {
        return Verdict::Keep(match (&worktree.done, &worktree.pushed) {
            (None, Some(remote)) if idle >= config.idle => Keep::Pushed(remote.clone(), idle),
            _ => Keep::Active(idle),
        });
    }
    match &worktree.checks {
        Checks::Pending => Verdict::Keep(Keep::Unchecked),
        Checks::Failed(blocker) => Verdict::Keep(Keep::Blocked(blocker.clone())),
        Checks::Passed => Verdict::Reap(match &worktree.done {
            Some(done) => Reason::Finished(done.clone(), idle),
            None => Reason::Idle(idle),
        }),
    }
}

pub fn plan(inventory: &Inventory, now: SystemTime, config: &Config, sweep: Sweep) -> Vec<Action> {
    let mut actions: Vec<Action> = inventory
        .trash
        .iter()
        .cloned()
        .map(Action::PurgeTrash)
        .collect();
    let idle_since = |time: SystemTime| now.duration_since(time).unwrap_or_default();

    // Output bases not deleted by an earlier rule.
    let mut kept = Vec::new();
    for base in &inventory.output_bases {
        match &base.workspace {
            Workspace::Missing(workspace) => actions.push(Action::DeleteOutputBase {
                base: base.clone(),
                reason: Reason::WorkspaceDeleted(workspace.clone()),
            }),
            _ => kept.push(base),
        }
    }
    let Sweep::Full { pressure } = sweep else {
        return actions;
    };

    for install in &inventory.install_bases {
        let idle = idle_since(install.last_used);
        if idle >= config.install_base_idle {
            actions.push(Action::DeleteInstallBase {
                base: install.clone(),
                reason: Reason::Idle(idle),
            });
        }
    }
    kept.retain(|&base| {
        let idle = idle_since(base.last_used);
        let expired = idle >= config.output_base_idle;
        if expired {
            actions.push(Action::DeleteOutputBase {
                base: base.clone(),
                reason: Reason::Idle(idle),
            });
        }
        !expired
    });

    let expiry = before(now, config.cache_entry_idle);
    let mut cutoffs: HashMap<&Path, SystemTime> = HashMap::new();
    let (idle_targets, live_targets): (Vec<&Target>, Vec<&Target>) = inventory
        .targets
        .iter()
        .partition(|target| idle_since(target.last_used) >= config.target_idle);
    let mut evicted_targets: HashSet<&Path> = HashSet::new();
    if let Some(pressure) = pressure {
        let usage = &pressure.usage;
        let floor = before(now, config.recent_use_floor);
        let mut freed: u64 = actions
            .iter()
            .map(|action| match action {
                Action::DeleteOutputBase { base, .. } => usage.size(&base.path),
                _ => 0,
            })
            .sum();
        freed += idle_targets
            .iter()
            .map(|target| usage.size(&target.root))
            .sum::<u64>();
        freed += live_targets
            .iter()
            .map(|target| usage.garbage(&target.root))
            .sum::<u64>();

        // Every candidate, keyed by the time it was last used.
        let mut queue: Vec<(SystemTime, Candidate)> = kept
            .iter()
            .filter(|base| base.last_used < floor && usage.sizes.contains_key(&base.path))
            .map(|base| (base.last_used, Candidate::OutputBase(base)))
            .collect();
        queue.extend(
            live_targets
                .iter()
                .filter(|target| target.last_used < floor && usage.sizes.contains_key(&target.root))
                .map(|target| (target.last_used, Candidate::Target(target))),
        );
        for cache in &inventory.caches {
            let Some(ages) = usage.ages.get(&cache.root) else {
                continue;
            };
            for (&start, &bytes) in ages {
                let end = start + AGE_BUCKET;
                if end <= expiry {
                    freed += bytes;
                } else if end <= floor {
                    queue.push((
                        start,
                        Candidate::Entries {
                            root: &cache.root,
                            end,
                            bytes,
                        },
                    ));
                }
            }
        }
        queue.sort_by_key(|(time, _)| *time);

        for (_, candidate) in queue {
            if freed >= pressure.bytes_to_free {
                break;
            }
            match candidate {
                Candidate::OutputBase(base) => {
                    freed += usage.size(&base.path);
                    actions.push(Action::DeleteOutputBase {
                        base: base.clone(),
                        reason: Reason::DiskPressure,
                    });
                }
                Candidate::Entries { root, end, bytes } => {
                    freed += bytes;
                    cutoffs.insert(root, end);
                }
                Candidate::Target(target) => {
                    // What the trim would not have freed already.
                    freed += usage
                        .size(&target.root)
                        .saturating_sub(usage.garbage(&target.root));
                    evicted_targets.insert(&target.root);
                }
            }
        }
    }

    for cache in &inventory.caches {
        let action = match cutoffs.get(cache.root.as_path()) {
            Some(&cutoff) => Action::EvictEntries {
                cache: cache.clone(),
                older_than: cutoff,
                reason: Reason::DiskPressure,
            },
            _ => Action::EvictEntries {
                cache: cache.clone(),
                older_than: expiry,
                reason: Reason::Idle(config.cache_entry_idle),
            },
        };
        actions.push(action);
    }

    // A target in a worktree about to go goes with it.
    let mut reaped: Vec<&Path> = Vec::new();
    let mut worktree_actions = Vec::new();
    if config.worktrees.enabled {
        for worktree in &inventory.worktrees {
            if let Verdict::Reap(reason) = verdict(worktree, now, &config.worktrees) {
                reaped.push(&worktree.path);
                worktree_actions.push(Action::ReapWorktree {
                    worktree: Box::new(worktree.clone()),
                    reason,
                });
            }
        }
        worktree_actions.extend(
            inventory
                .prunable
                .iter()
                .cloned()
                .map(Action::PruneWorktree),
        );
    }
    let in_reaped = |target: &Target| reaped.iter().any(|path| target.root.starts_with(path));

    for target in idle_targets.into_iter().filter(|target| !in_reaped(target)) {
        actions.push(Action::DeleteTarget {
            target: target.clone(),
            reason: Reason::Idle(idle_since(target.last_used)),
        });
    }
    for target in live_targets.into_iter().filter(|target| !in_reaped(target)) {
        actions.push(if evicted_targets.contains(target.root.as_path()) {
            Action::DeleteTarget {
                target: target.clone(),
                reason: Reason::DiskPressure,
            }
        } else {
            Action::TrimTarget(target.clone())
        });
    }
    actions.extend(worktree_actions);
    actions
}

fn before(now: SystemTime, duration: Duration) -> SystemTime {
    now.checked_sub(duration).unwrap_or(SystemTime::UNIX_EPOCH)
}

enum Candidate<'a> {
    OutputBase(&'a OutputBase),
    Target(&'a Target),
    /// The disk-cache entries in the age bucket that ends at `end`.
    Entries {
        root: &'a Path,
        end: SystemTime,
        bytes: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{CacheKind, bucket_start};
    use std::collections::BTreeMap;

    const HOUR: Duration = Duration::from_secs(3600);

    fn now() -> SystemTime {
        bucket_start(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
    }

    fn base(name: &str, workspace: Workspace, idle: Duration) -> OutputBase {
        OutputBase {
            path: PathBuf::from(format!("/root/{name}")),
            workspace,
            last_used: now() - idle,
        }
    }

    fn present() -> Workspace {
        Workspace::Present("/work/repo".into())
    }

    fn disk_cache() -> Cache {
        Cache {
            root: "/cache/disk".into(),
            kind: CacheKind::Disk,
        }
    }

    fn deleted_bases(actions: &[Action]) -> Vec<(&str, &Reason)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::DeleteOutputBase { base, reason } => Some((base.path.to_str()?, reason)),
                _ => None,
            })
            .collect()
    }

    fn eviction(actions: &[Action]) -> (SystemTime, &Reason) {
        actions
            .iter()
            .find_map(|action| match action {
                Action::EvictEntries {
                    older_than, reason, ..
                } => Some((*older_than, reason)),
                _ => None,
            })
            .unwrap()
    }

    fn target(name: &str, idle: Duration) -> Target {
        Target {
            root: PathBuf::from(format!("/src/{name}/target")),
            last_used: now() - idle,
        }
    }

    fn target_actions(actions: &[Action]) -> Vec<(&str, Option<&Reason>)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::TrimTarget(target) => Some((target.root.to_str()?, None)),
                Action::DeleteTarget { target, reason } => {
                    Some((target.root.to_str()?, Some(reason)))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn targets_are_trimmed_on_full_sweeps_and_deleted_once_idle() {
        let inventory = Inventory {
            targets: vec![target("active", HOUR), target("abandoned", 8 * 24 * HOUR)],
            ..Inventory::default()
        };
        let config = Config::default();
        assert!(target_actions(&plan(&inventory, now(), &config, Sweep::Orphans)).is_empty());

        let actions = plan(&inventory, now(), &config, Sweep::Full { pressure: None });
        assert_eq!(
            target_actions(&actions),
            [
                ("/src/abandoned/target", Some(&Reason::Idle(8 * 24 * HOUR))),
                ("/src/active/target", None),
            ]
        );
    }

    #[test]
    fn pressure_counts_trimmed_garbage_before_deleting_whole_targets() {
        let inventory = Inventory {
            targets: vec![target("older", 10 * HOUR), target("newer", 5 * HOUR)],
            ..Inventory::default()
        };
        let usage = Usage {
            sizes: HashMap::from([
                ("/src/older/target".into(), 1000),
                ("/src/newer/target".into(), 1000),
            ]),
            garbage: HashMap::from([("/src/older/target".into(), 900)]),
            ..Usage::default()
        };
        let sweep = |bytes_to_free| Sweep::Full {
            pressure: Some(Pressure {
                bytes_to_free,
                usage: usage.clone(),
            }),
        };
        let config = Config::default();

        let actions = plan(&inventory, now(), &config, sweep(800));
        assert_eq!(
            target_actions(&actions),
            [("/src/older/target", None), ("/src/newer/target", None)]
        );

        let actions = plan(&inventory, now(), &config, sweep(1000));
        assert_eq!(
            target_actions(&actions),
            [
                ("/src/older/target", Some(&Reason::DiskPressure)),
                ("/src/newer/target", None),
            ]
        );
    }

    #[test]
    fn orphans_go_on_every_sweep_but_unreachable_workspaces_stay() {
        let inventory = Inventory {
            output_bases: vec![
                base("gone", Workspace::Missing("/w/gone".into()), Duration::ZERO),
                base(
                    "volume",
                    Workspace::Unreachable("/Volumes/x/w".into()),
                    Duration::ZERO,
                ),
                base("live", present(), Duration::ZERO),
            ],
            trash: vec!["/root/.cendrillon-trash-old".into()],
            ..Inventory::default()
        };
        let actions = plan(&inventory, now(), &Config::default(), Sweep::Orphans);
        assert_eq!(
            actions,
            [
                Action::PurgeTrash("/root/.cendrillon-trash-old".into()),
                Action::DeleteOutputBase {
                    base: inventory.output_bases[0].clone(),
                    reason: Reason::WorkspaceDeleted("/w/gone".into()),
                },
            ]
        );
    }

    #[test]
    fn idle_deadlines_apply_only_on_full_sweeps() {
        let inventory = Inventory {
            output_bases: vec![
                base("stale", present(), 25 * HOUR),
                base("recent", present(), 23 * HOUR),
            ],
            install_bases: vec![InstallBase {
                path: "/root/install/old".into(),
                last_used: now() - 73 * HOUR,
            }],
            caches: vec![disk_cache()],
            ..Inventory::default()
        };
        let config = Config::default();
        assert!(deleted_bases(&plan(&inventory, now(), &config, Sweep::Orphans)).is_empty());

        let actions = plan(&inventory, now(), &config, Sweep::Full { pressure: None });
        assert_eq!(
            deleted_bases(&actions),
            [("/root/stale", &Reason::Idle(25 * HOUR))]
        );
        assert!(actions.contains(&Action::DeleteInstallBase {
            base: inventory.install_bases[0].clone(),
            reason: Reason::Idle(73 * HOUR),
        }));
        assert_eq!(
            eviction(&actions),
            (now() - 72 * HOUR, &Reason::Idle(72 * HOUR))
        );
    }

    #[test]
    fn pressure_evicts_least_recently_used_first_until_enough_is_freed() {
        let cache = disk_cache();
        let inventory = Inventory {
            output_bases: vec![
                base("orphan", Workspace::Missing("/w/x".into()), HOUR),
                base("ten_hours", present(), 10 * HOUR),
                base("five_hours", present(), 5 * HOUR),
                base("one_hour", present(), HOUR),
            ],
            caches: vec![cache.clone()],
            ..Inventory::default()
        };
        let usage = Usage {
            sizes: HashMap::from([
                ("/root/orphan".into(), 100),
                ("/root/ten_hours".into(), 1000),
                ("/root/five_hours".into(), 1000),
                ("/root/one_hour".into(), 1000),
            ]),
            ages: HashMap::from([(
                cache.root.clone(),
                BTreeMap::from([
                    (now() - 100 * HOUR, 50), // already past the idle deadline
                    (now() - 20 * HOUR, 400),
                    (now() - 8 * HOUR, 400),
                    (now() - 3 * HOUR, 400),
                ]),
            )]),
            ..Usage::default()
        };
        let pressure = Pressure {
            // orphan + expired entries (150) + 20h and 10h-old items (1400) fall short by 50;
            // the 8h-old entries cover it.
            bytes_to_free: 1600,
            usage,
        };
        let actions = plan(
            &inventory,
            now(),
            &Config::default(),
            Sweep::Full {
                pressure: Some(pressure),
            },
        );
        assert_eq!(
            deleted_bases(&actions),
            [
                ("/root/orphan", &Reason::WorkspaceDeleted("/w/x".into())),
                ("/root/ten_hours", &Reason::DiskPressure),
            ]
        );
        assert_eq!(
            eviction(&actions),
            (now() - 8 * HOUR + AGE_BUCKET, &Reason::DiskPressure)
        );
    }

    #[test]
    fn pressure_skips_items_on_other_disks() {
        let inventory = Inventory {
            output_bases: vec![base("external", present(), 50 * HOUR)],
            ..Inventory::default()
        };
        let config = Config {
            output_base_idle: 100 * HOUR,
            ..Config::default()
        };
        let pressure = Pressure {
            bytes_to_free: 1000,
            usage: Usage::default(),
        };
        let actions = plan(
            &inventory,
            now(),
            &config,
            Sweep::Full {
                pressure: Some(pressure),
            },
        );
        assert!(deleted_bases(&actions).is_empty());
    }

    fn linked(name: &str, idle: Duration, done: Option<Done>, checks: Checks) -> Worktree {
        Worktree {
            path: PathBuf::from(format!("/src/{name}")),
            repo: worktree::Repo {
                git_dir: "/src/repo/.git".into(),
                dir: "/src/repo".into(),
                default_branch: Some("refs/remotes/origin/main".into()),
            },
            admin: Some(format!("/src/repo/.git/worktrees/{name}").into()),
            head: Some("1111".into()),
            branch: Some(format!("refs/heads/{name}")),
            locked: None,
            last_activity: now() - idle,
            done,
            pushed: None,
            output_bases: Vec::new(),
            targets: Vec::new(),
            checks,
        }
    }

    fn merged() -> Option<Done> {
        Some(Done::Merged("refs/remotes/origin/main".into()))
    }

    #[test]
    fn a_worktree_goes_only_when_every_condition_holds() {
        let config = WorktreeConfig::default();
        let verdict_of = |worktree: &Worktree| verdict(worktree, now(), &config);
        let day = 24 * HOUR;

        assert_eq!(
            verdict_of(&linked("idle", 8 * day, None, Checks::Passed)),
            Verdict::Reap(Reason::Idle(8 * day))
        );
        assert_eq!(
            verdict_of(&linked("merged", 2 * day, merged(), Checks::Passed)),
            Verdict::Reap(Reason::Finished(merged().unwrap(), 2 * day))
        );
        assert_eq!(
            verdict_of(&linked("unmerged", 2 * day, None, Checks::Passed)),
            Verdict::Keep(Keep::Active(2 * day))
        );
        assert_eq!(
            verdict_of(&linked("fresh", HOUR, merged(), Checks::Passed)),
            Verdict::Keep(Keep::Active(HOUR))
        );
        assert_eq!(
            verdict_of(&linked("pending", 8 * day, None, Checks::Pending)),
            Verdict::Keep(Keep::Unchecked)
        );
        let dirty = Checks::Failed(Blocker::Untracked);
        assert_eq!(
            verdict_of(&linked("dirty", 8 * day, None, dirty)),
            Verdict::Keep(Keep::Blocked(Blocker::Untracked))
        );
        let locked = Worktree {
            locked: Some("agent at work".into()),
            ..linked("locked", 30 * day, merged(), Checks::Passed)
        };
        assert_eq!(
            verdict_of(&locked),
            Verdict::Keep(Keep::Locked("agent at work".into()))
        );

        let pushed = |idle| Worktree {
            pushed: Some("origin/pr".into()),
            ..linked("pr", idle, None, Checks::Passed)
        };
        assert_eq!(
            verdict_of(&pushed(2 * day)),
            Verdict::Keep(Keep::Active(2 * day))
        );
        assert_eq!(
            verdict_of(&pushed(8 * day)),
            Verdict::Keep(Keep::Pushed("origin/pr".into(), 8 * day))
        );
        assert_eq!(
            verdict_of(&pushed(31 * day)),
            Verdict::Reap(Reason::Idle(31 * day))
        );
        let merged_and_pushed = Worktree {
            pushed: Some("origin/merged".into()),
            ..linked("merged", 2 * day, merged(), Checks::Passed)
        };
        assert_eq!(
            verdict_of(&merged_and_pushed),
            Verdict::Reap(Reason::Finished(merged().unwrap(), 2 * day))
        );

        let excluding = WorktreeConfig {
            exclude: vec!["/src/idle".into()],
            ..WorktreeConfig::default()
        };
        let idle = linked("idle", 8 * day, None, Checks::Passed);
        assert_eq!(
            verdict(&idle, now(), &excluding),
            Verdict::Keep(Keep::Excluded)
        );
    }

    #[test]
    fn worktrees_are_reaped_on_full_sweeps_when_enabled_with_their_targets() {
        let day = 24 * HOUR;
        let gone = linked("gone", 8 * day, None, Checks::Passed);
        let inventory = Inventory {
            worktrees: vec![gone.clone(), linked("kept", HOUR, None, Checks::Pending)],
            prunable: vec![Prunable {
                repo: gone.repo.clone(),
                path: "/src/deleted".into(),
            }],
            targets: vec![target("gone", HOUR), target("kept", HOUR)],
            ..Inventory::default()
        };
        let enabled = Config {
            worktrees: WorktreeConfig {
                enabled: true,
                ..WorktreeConfig::default()
            },
            ..Config::default()
        };
        let worktree_actions = |actions: &[Action]| -> Vec<String> {
            actions
                .iter()
                .filter_map(|action| match action {
                    Action::ReapWorktree { worktree, .. } => {
                        Some(format!("reap {}", worktree.path.display()))
                    }
                    Action::PruneWorktree(prunable) => {
                        Some(format!("prune {}", prunable.path.display()))
                    }
                    _ => None,
                })
                .collect()
        };

        let full = plan(&inventory, now(), &enabled, Sweep::Full { pressure: None });
        assert_eq!(
            worktree_actions(&full),
            ["reap /src/gone", "prune /src/deleted"]
        );
        // The target in the reaped worktree goes with it.
        assert_eq!(target_actions(&full), [("/src/kept/target", None)]);

        let orphans = plan(&inventory, now(), &enabled, Sweep::Orphans);
        assert!(worktree_actions(&orphans).is_empty());
        let disabled = plan(
            &inventory,
            now(),
            &Config::default(),
            Sweep::Full { pressure: None },
        );
        assert!(worktree_actions(&disabled).is_empty());
        assert_eq!(target_actions(&disabled).len(), 2);
    }

    #[test]
    fn pressure_never_touches_recently_used_items() {
        let cache = disk_cache();
        let inventory = Inventory {
            output_bases: vec![base("one_hour", present(), HOUR)],
            caches: vec![cache.clone()],
            ..Inventory::default()
        };
        let usage = Usage {
            sizes: HashMap::from([("/root/one_hour".into(), 1000)]),
            ages: HashMap::from([(cache.root.clone(), BTreeMap::from([(now() - HOUR, 1000)]))]),
            ..Usage::default()
        };
        let actions = plan(
            &inventory,
            now(),
            &Config::default(),
            Sweep::Full {
                pressure: Some(Pressure {
                    bytes_to_free: u64::MAX,
                    usage,
                }),
            },
        );
        assert!(deleted_bases(&actions).is_empty());
        assert_eq!(
            eviction(&actions),
            (now() - 72 * HOUR, &Reason::Idle(72 * HOUR))
        );
    }
}
