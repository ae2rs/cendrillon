//! Linked git worktrees nobody uses any more: finding them through the repositories that own
//! them, and gathering what decides whether removing one loses anything. The decision itself
//! is the planner's ([`crate::planner::verdict`]).

use std::cell::OnceCell;
use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use nix::unistd::Pid;
use walkdir::WalkDir;

use crate::cargo::Target;
use crate::config::WorktreeConfig;
use crate::discovery::in_skipped_home_dir;
use crate::git::{self, Entry};
use crate::inventory::{self, Inventory, Workspace};
use crate::planner::{Keep, Verdict, verdict};
use crate::sys;

/// Files in a worktree's administrative directory that tell an operation is under way.
const OPERATIONS: [(&str, &str); 6] = [
    ("MERGE_HEAD", "a merge"),
    ("rebase-merge", "a rebase"),
    ("rebase-apply", "a rebase"),
    ("CHERRY_PICK_HEAD", "a cherry-pick"),
    ("REVERT_HEAD", "a revert"),
    ("BISECT_LOG", "a bisect"),
];

/// A repository with linked worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Its common git directory, `<main worktree>/.git` unless bare.
    pub git_dir: PathBuf,
    /// Where repository-wide git commands run: the main worktree, or the git directory of a
    /// bare repository.
    pub dir: PathBuf,
    /// The branch a finished branch is merged into: `origin/HEAD`, else `origin/main` or
    /// `origin/master` (or another remote's), else the local `main` or `master`.
    pub default_branch: Option<String>,
}

impl Repo {
    /// Asks git for the worktrees of the repository at `git_dir`, the main one first.
    pub fn list(git_dir: &Path) -> Result<(Self, Vec<Entry>)> {
        let listed = git::output(git_dir, &["worktree", "list", "--porcelain", "-z"])?;
        let entries = git::parse_worktrees(&listed);
        let main = entries.first().context("git listed no worktree")?;
        let dir = match main.bare {
            true => git_dir.to_path_buf(),
            false => main.path.clone(),
        };
        let default_branch = default_branch(&dir).unwrap_or_else(|e| {
            tracing::debug!("finding the default branch of {}: {e:#}", git_dir.display());
            None
        });
        let repo = Self {
            git_dir: git_dir.to_path_buf(),
            dir,
            default_branch,
        };
        Ok((repo, entries))
    }
}

fn default_branch(dir: &Path) -> Result<Option<String>> {
    let listed = git::text(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(symref)",
            "refs/remotes/*/HEAD",
            "refs/remotes/*/main",
            "refs/remotes/*/master",
            "refs/heads/main",
            "refs/heads/master",
        ],
    )?;
    let refs: Vec<(&str, &str)> = listed
        .lines()
        .map(|line| line.split_once(' ').unwrap_or((line, "")))
        .collect();
    let mut remotes: Vec<&str> = refs
        .iter()
        .filter_map(|(name, _)| name.strip_prefix("refs/remotes/")?.split_once('/'))
        .map(|(remote, _)| remote)
        .collect();
    remotes.sort_by_key(|remote| (*remote != "origin", *remote));
    remotes.dedup();
    let has = |name: &str| refs.iter().any(|(found, _)| *found == name);
    for remote in &remotes {
        let head = format!("refs/remotes/{remote}/HEAD");
        if let Some((_, target)) = refs.iter().find(|(name, _)| *name == head)
            && !target.is_empty()
        {
            return Ok(Some(target.to_string()));
        }
        for branch in ["main", "master"] {
            let name = format!("refs/remotes/{remote}/{branch}");
            if has(&name) {
                return Ok(Some(name));
            }
        }
    }
    if remotes.is_empty() {
        return Ok(["refs/heads/main", "refs/heads/master"]
            .into_iter()
            .find(|name| has(name))
            .map(str::to_owned));
    }
    Ok(None)
}

/// Why a worktree's branch counts as finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    /// HEAD is in the default branch.
    Merged(String),
    /// The branch's upstream is configured but was deleted, as hosts do on merging.
    UpstreamGone(String),
}

impl fmt::Display for Done {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Done::Merged(into) => write!(f, "merged into {}", short_ref(into)),
            Done::UpstreamGone(upstream) => write!(f, "upstream {upstream} is gone"),
        }
    }
}

/// What keeps a worktree that nothing else does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    Changes,
    Untracked,
    Operation(&'static str),
    Submodules,
    Protected(String),
    Nested(PathBuf),
    InUse(Pid),
    Unchecked(String),
}

impl Blocker {
    /// A few words, to count worktrees by why they stay.
    pub fn label(&self) -> &'static str {
        match self {
            Blocker::Changes => "uncommitted changes",
            Blocker::Untracked => "untracked files",
            Blocker::Operation(_) => "operation in progress",
            Blocker::Submodules => "submodules",
            Blocker::Protected(_) => "protected ignored files",
            Blocker::Nested(_) => "another checkout inside",
            Blocker::InUse(_) => "in use",
            Blocker::Unchecked(_) => "check failed",
        }
    }
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::Changes => f.write_str("has uncommitted changes"),
            Blocker::Untracked => f.write_str("has untracked files"),
            Blocker::Operation(what) => write!(f, "{what} is in progress"),
            Blocker::Submodules => f.write_str("has submodules, which git will not remove"),
            Blocker::Protected(path) => write!(f, "has protected ignored file {path}"),
            Blocker::Nested(path) => write!(f, "holds another git checkout at {}", path.display()),
            Blocker::InUse(pid) => write!(f, "in use by pid {pid}"),
            Blocker::Unchecked(why) => write!(f, "cannot check it: {why}"),
        }
    }
}

/// The checks that need `git status` and the process table, run only when nothing cheaper
/// already keeps the worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checks {
    Pending,
    Passed,
    Failed(Blocker),
}

/// A linked worktree, as git and the file system describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub repo: Repo,
    /// Its administrative directory, `<git dir>/worktrees/<name>`, from its `.git` file.
    pub admin: Option<PathBuf>,
    pub head: Option<String>,
    pub branch: Option<String>,
    /// Why git keeps it, possibly empty, when locked.
    pub locked: Option<String>,
    /// The latest of: its HEAD, index and HEAD log changing, an entry added to or removed from
    /// its top directory, a Bazel command in its output bases, Cargo using a target in it.
    pub last_activity: SystemTime,
    /// Known only once it is idle for long enough to matter.
    pub done: Option<Done>,
    /// The remote branch that still holds its unfinished branch, typically that of an open
    /// pull request. Known only once it is idle for long enough to matter.
    pub pushed: Option<String>,
    /// Output bases of a Bazel workspace here.
    pub output_bases: Vec<PathBuf>,
    /// Cargo targets inside.
    pub targets: Vec<PathBuf>,
    pub checks: Checks,
}

impl Worktree {
    /// Its name in git, that of its administrative directory.
    pub fn name(&self) -> String {
        self.admin
            .as_deref()
            .or(Some(&self.path))
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// The record of a worktree whose directory is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prunable {
    pub repo: Repo,
    pub path: PathBuf,
}

/// Linked worktrees of the given repositories, and records of deleted ones.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Survey {
    pub worktrees: Vec<Worktree>,
    pub prunable: Vec<Prunable>,
}

/// Lists the worktrees of `repos` and inspects each linked one. Those in folders of `home`
/// never searched are left out, with their repositories.
pub fn survey(
    repos: &BTreeSet<PathBuf>,
    inventory: &Inventory,
    now: SystemTime,
    config: &WorktreeConfig,
    home: &Path,
) -> Survey {
    if git::binary().is_none() {
        tracing::warn!("git not found in PATH, /opt/homebrew/bin, /usr/local/bin or /usr/bin");
        return Survey::default();
    }
    let processes = OnceCell::new();
    let processes = || processes.get_or_init(sys::open_paths).as_slice();
    let mut seen = HashSet::new();
    let mut survey = Survey::default();
    for git_dir in repos.iter().filter(|dir| !in_skipped_home_dir(dir, home)) {
        let (repo, entries) = match Repo::list(git_dir) {
            Ok(listed) => listed,
            Err(e) => {
                tracing::warn!("listing the worktrees of {}: {e:#}", git_dir.display());
                continue;
            }
        };
        // The first is the main worktree, or the repository itself when bare.
        for entry in entries.iter().skip(1) {
            if entry.bare
                || in_skipped_home_dir(&entry.path, home)
                || !seen.insert(entry.path.clone())
            {
                continue;
            }
            if entry.prunable {
                // A worktree on a removable drive is locked, by git's convention; one on a
                // volume not mounted right now is left alone too. Its HEAD is read again
                // before the record goes.
                if inventory::is_deleted(&entry.path) && entry.locked.is_none() {
                    survey.prunable.push(Prunable {
                        repo: repo.clone(),
                        path: entry.path.clone(),
                    });
                }
                continue;
            }
            let links = Links::from_inventory(&entry.path, inventory);
            let worktree = inspect(&repo, entry, &links, now, config, false, processes);
            survey.worktrees.push(worktree);
        }
    }
    survey
}

/// What else cendrillon knows that lives in a worktree, with when it was last used.
#[derive(Debug, Default)]
pub struct Links {
    pub output_bases: Vec<(PathBuf, SystemTime)>,
    pub targets: Vec<(PathBuf, SystemTime)>,
}

impl Links {
    fn from_inventory(path: &Path, inventory: &Inventory) -> Self {
        let path = canonical(path);
        let output_bases = inventory
            .output_bases
            .iter()
            .filter(|base| match &base.workspace {
                Workspace::Present(workspace) => canonical(workspace) == path,
                _ => false,
            })
            .map(|base| (base.path.clone(), base.last_used))
            .collect();
        let targets = inventory
            .targets
            .iter()
            .filter(|target| target.root.starts_with(&path))
            .map(|target| (target.root.clone(), target.last_used))
            .collect();
        Self {
            output_bases,
            targets,
        }
    }

    /// The same, read again from disk.
    pub fn reread(worktree: &Worktree) -> Self {
        let output_bases = worktree
            .output_bases
            .iter()
            .filter_map(|base| Some((base.clone(), inventory::last_used(base).ok()?)))
            .collect();
        let targets = worktree
            .targets
            .iter()
            .filter_map(|root| Some((root.clone(), Target::classify(root)?.last_used)))
            .collect();
        Self {
            output_bases,
            targets,
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Gathers what decides the fate of one linked worktree. The slower checks run only when the
/// planner would otherwise remove it, or when `force_checks`.
pub fn inspect<'a>(
    repo: &Repo,
    entry: &Entry,
    links: &Links,
    now: SystemTime,
    config: &WorktreeConfig,
    force_checks: bool,
    processes: impl Fn() -> &'a [(Pid, PathBuf)],
) -> Worktree {
    let path = canonical(&entry.path);
    let admin = git::admin_dir(&path);
    let mut times: Vec<SystemTime> = admin
        .iter()
        .flat_map(|admin| ["HEAD", "index", "logs/HEAD"].map(|file| admin.join(file)))
        .chain(std::iter::once(path.clone()))
        .filter_map(|file| fs::symlink_metadata(file).ok()?.modified().ok())
        .collect();
    times.extend(links.output_bases.iter().map(|(_, time)| *time));
    times.extend(links.targets.iter().map(|(_, time)| *time));
    // Nothing to date it by: count it as used now.
    let last_activity = times.into_iter().max().unwrap_or(now);
    let idle = now.duration_since(last_activity).unwrap_or_default();
    let mut worktree = Worktree {
        path,
        repo: repo.clone(),
        admin,
        head: entry.head.clone(),
        branch: entry.branch.clone(),
        locked: entry.locked.clone(),
        last_activity,
        done: None,
        pushed: None,
        output_bases: links.output_bases.iter().map(|(p, _)| p.clone()).collect(),
        targets: links.targets.iter().map(|(p, _)| p.clone()).collect(),
        checks: Checks::Pending,
    };
    if force_checks || idle >= config.idle.min(config.idle_when_merged) {
        worktree.done = done(&worktree).unwrap_or_else(|e| {
            tracing::debug!("checking if {} is merged: {e:#}", worktree.path.display());
            None
        });
        if worktree.done.is_none() {
            worktree.pushed = pushed(&worktree).unwrap_or_else(|e| {
                tracing::debug!("checking if {} is pushed: {e:#}", worktree.path.display());
                None
            });
        }
    }
    if force_checks || verdict(&worktree, now, config) == Verdict::Keep(Keep::Unchecked) {
        worktree.checks = match checks(&worktree, config, processes) {
            Ok(None) => Checks::Passed,
            Ok(Some(blocker)) => Checks::Failed(blocker),
            Err(e) => Checks::Failed(Blocker::Unchecked(format!("{e:#}"))),
        };
    }
    worktree
}

/// Whether the worktree's branch is finished: its upstream deleted, or its HEAD merged into
/// the default branch (which is not itself).
fn done(worktree: &Worktree) -> Result<Option<Done>> {
    let dir = &worktree.repo.dir;
    if let Some(branch) = &worktree.branch {
        let track = git::text(
            dir,
            &[
                "for-each-ref",
                "--format=%(upstream:short)%00%(upstream:track)",
                branch,
            ],
        )?;
        if let Some((upstream, "[gone]")) = track.split_once('\0') {
            return Ok(Some(Done::UpstreamGone(upstream.to_owned())));
        }
    }
    let (Some(head), Some(default)) = (&worktree.head, &worktree.repo.default_branch) else {
        return Ok(None);
    };
    if worktree.branch.as_ref() == Some(default) {
        return Ok(None);
    }
    let mut command = git::command(dir)?;
    command.args(["merge-base", "--is-ancestor", head, default]);
    let ran = git::run(command, git::TIMEOUT)?;
    match ran.status.code() {
        Some(0) => Ok(Some(Done::Merged(default.clone()))),
        Some(1) => Ok(None),
        _ => Err(anyhow::anyhow!(ran.message())),
    }
}

/// The remote branch still holding the worktree's branch, as last fetched: its upstream unless
/// gone, or else a branch of the same name on any remote. Asked only of branches not done.
/// An upstream that is a shared branch does not count: `git worktree add -b x origin/main`
/// makes `origin/main` the upstream of a branch never pushed.
fn pushed(worktree: &Worktree) -> Result<Option<String>> {
    let Some(branch) = &worktree.branch else {
        return Ok(None);
    };
    let dir = &worktree.repo.dir;
    let track = git::text(
        dir,
        &[
            "for-each-ref",
            "--format=%(upstream)%00%(upstream:track)",
            branch,
        ],
    )?;
    if let Some((upstream, track)) = track.split_once('\0')
        && upstream.starts_with("refs/remotes/")
        && track != "[gone]"
        && !is_shared(upstream, worktree.repo.default_branch.as_deref())
    {
        return Ok(Some(short_ref(upstream).to_owned()));
    }
    let Some(name) = branch.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    let found = git::text(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/remotes/*/{name}"),
        ],
    )?;
    // The glob's `*` may span slashes: keep `refs/remotes/<remote>/<name>` only.
    Ok(found
        .lines()
        .map(short_ref)
        .find(|short| {
            short
                .strip_suffix(name)
                .and_then(|remote| remote.strip_suffix('/'))
                .is_some_and(|remote| !remote.is_empty() && !remote.contains('/'))
        })
        .map(str::to_owned))
}

/// Whether a remote branch is one everybody works from: the default branch, `main` or
/// `master`.
fn is_shared(upstream: &str, default: Option<&str>) -> bool {
    if Some(upstream) == default {
        return true;
    }
    let name = short_ref(upstream)
        .split_once('/')
        .map_or("", |(_, name)| name);
    matches!(name, "main" | "master")
}

/// The first reason found to keep the worktree, cheapest checks first.
fn checks<'a>(
    worktree: &Worktree,
    config: &WorktreeConfig,
    processes: impl Fn() -> &'a [(Pid, PathBuf)],
) -> Result<Option<Blocker>> {
    let admin = worktree
        .admin
        .as_deref()
        .context("its .git file does not lead to the repository")?;
    for (file, what) in OPERATIONS {
        if fs::symlink_metadata(admin.join(file)).is_ok() {
            return Ok(Some(Blocker::Operation(what)));
        }
    }
    if [worktree.path.join(".gitmodules"), admin.join("modules")]
        .iter()
        .any(|path| fs::symlink_metadata(path).is_ok())
    {
        return Ok(Some(Blocker::Submodules));
    }
    if let Some(pid) = user(worktree, processes()) {
        return Ok(Some(Blocker::InUse(pid)));
    }
    let listed = git::output(
        &worktree.path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    let status = git::parse_status(&listed);
    if status.changes {
        return Ok(Some(Blocker::Changes));
    }
    if status.untracked {
        return Ok(Some(Blocker::Untracked));
    }
    for ignored in &status.ignored {
        let relative = ignored.trim_end_matches('/');
        if is_protected(relative, &config.protect) {
            return Ok(Some(Blocker::Protected(ignored.clone())));
        }
        let nested = worktree.path.join(relative).join(".git");
        if ignored.ends_with('/') && fs::symlink_metadata(&nested).is_ok() {
            return Ok(Some(Blocker::Nested(worktree.path.join(relative))));
        }
    }
    Ok(None)
}

/// A process using something in the worktree. A Bazel server of its own output base does not
/// count: it idles as long as no command runs, which the last use of the output base tells,
/// and it is stopped when the output base goes once the worktree is gone.
fn user(worktree: &Worktree, processes: &[(Pid, PathBuf)]) -> Option<Pid> {
    let servers: Vec<Pid> = worktree
        .output_bases
        .iter()
        .filter_map(|base| sys::bazel_server(base))
        .collect();
    processes
        .iter()
        .find(|(pid, path)| path.starts_with(&worktree.path) && !servers.contains(pid))
        .map(|(pid, _)| *pid)
}

/// Whether an ignored path matches a protected glob: on its name for a glob without a slash,
/// else on its path in the worktree.
pub fn is_protected(relative: &str, globs: &[String]) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    globs.iter().any(|glob| match glob.contains('/') {
        true => git::glob_match(glob.trim_start_matches('/'), relative),
        false => git::glob_match(glob, name),
    })
}

/// Whether the worktree at `path` is excluded: a glob with a slash matches its path or a
/// parent's, one without matches its directory name.
pub fn is_excluded(path: &Path, globs: &[String]) -> bool {
    globs.iter().any(|glob| match glob.contains('/') {
        true => path
            .ancestors()
            .any(|dir| git::glob_match(glob.trim_end_matches('/'), &dir.to_string_lossy())),
        false => path
            .file_name()
            .is_some_and(|name| git::glob_match(glob, &name.to_string_lossy())),
    })
}

/// Bytes allocated under the worktree, and the first other checkout found inside it: a `.git`
/// below its top directory, which removing the worktree would delete too.
pub fn measure(path: &Path) -> (u64, Option<PathBuf>) {
    let mut seen = HashSet::new();
    let mut nested = None;
    let mut size = 0;
    for entry in WalkDir::new(path)
        .follow_root_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if nested.is_none() && entry.depth() > 1 && entry.file_name() == ".git" {
            nested = entry.path().parent().map(Path::to_path_buf);
        }
        if let Ok(meta) = entry.metadata() {
            size += sys::allocated(&meta, &mut seen);
        }
    }
    (size, nested)
}

/// Reads the worktree at `path` again, with every check, for a last look before removing it.
/// `None` when git no longer lists it as a linked worktree of the same repository.
pub fn reinspect(
    planned: &Worktree,
    now: SystemTime,
    config: &WorktreeConfig,
) -> Result<Option<Worktree>> {
    let (repo, entries) = Repo::list(&planned.repo.git_dir)?;
    let Some(entry) = entries
        .iter()
        .skip(1)
        .find(|entry| !entry.bare && !entry.prunable && canonical(&entry.path) == planned.path)
    else {
        return Ok(None);
    };
    let processes = OnceCell::new();
    let processes = || processes.get_or_init(sys::open_paths).as_slice();
    let links = Links::reread(planned);
    let worktree = inspect(&repo, entry, &links, now, config, true, processes);
    // Only a linked worktree of this repository has its `.git` file lead there.
    let linked = worktree
        .admin
        .as_deref()
        .and_then(git::common_dir)
        .is_some_and(|common| common == planned.repo.git_dir);
    Ok(linked.then_some(worktree))
}

/// The detached HEAD of a worktree when no branch, tag or remote-tracking branch holds it, so
/// removing the worktree, or its record, would leave its commits only in its reflog, which
/// goes too.
pub fn unreachable_head<'a>(
    repo: &Repo,
    head: Option<&'a str>,
    branch: Option<&str>,
) -> Result<Option<&'a str>> {
    let Some(head) = head.filter(|_| branch.is_none()) else {
        return Ok(None);
    };
    let containing = git::text(
        &repo.dir,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "--contains",
            head,
            "refs/heads",
            "refs/tags",
            "refs/remotes",
        ],
    )?;
    Ok(containing.is_empty().then_some(head))
}

/// Where a detached HEAD is kept when its worktree goes: `refs/cendrillon/reaped/<name>-<unix
/// time>`.
pub fn keep_ref(name: &str, now: SystemTime) -> String {
    let name: String = name
        .chars()
        .map(|c| match c.is_ascii_alphanumeric() || "-_".contains(c) {
            true => c,
            false => '-',
        })
        .collect();
    let stamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("refs/cendrillon/reaped/{name}-{stamp}")
}

/// `origin/main` for `refs/remotes/origin/main`, `main` for `refs/heads/main`.
pub fn short_ref(name: &str) -> &str {
    name.strip_prefix("refs/remotes/")
        .or_else(|| name.strip_prefix("refs/heads/"))
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixtures::{git, repo, worktree};
    use crate::inventory::fixtures::set_mtime;
    use std::time::Duration;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    #[test]
    fn protected_and_excluded_globs() {
        let protect = WorktreeConfig::default().protect;
        assert!(is_protected(".env", &protect));
        assert!(is_protected("services/api/.env", &protect));
        assert!(is_protected("services/.env.production", &protect));
        assert!(is_protected(".claude/settings.local.json", &protect));
        assert!(is_protected("user.bazelrc", &protect));
        assert!(!is_protected("target", &protect));
        assert!(!is_protected("app.log", &protect));
        assert!(is_protected("secrets/key", &["secrets/*".into()]));

        let exclude = ["/home/me/keep".to_string(), "pinned-*".to_string()];
        assert!(is_excluded(Path::new("/home/me/keep"), &exclude));
        assert!(is_excluded(Path::new("/home/me/keep/wt"), &exclude));
        assert!(is_excluded(Path::new("/tmp/pinned-1"), &exclude));
        assert!(!is_excluded(Path::new("/home/me/keeper"), &exclude));
        assert!(!is_excluded(Path::new("/home/me/x"), &[]));
    }

    #[test]
    fn finds_the_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        let local = repo(dir.path());
        assert_eq!(
            default_branch(&local).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        let clone = dir.path().join("clone");
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                local.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        assert_eq!(
            default_branch(&clone).unwrap().as_deref(),
            Some("refs/remotes/origin/main")
        );
    }

    #[test]
    fn leaves_out_worktrees_and_repositories_in_folders_never_searched() {
        let dir = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        let skipped = home.join(crate::discovery::SKIPPED_HOME_DIRS[0]);
        let repo_dir = repo(&home);
        fs::create_dir_all(&skipped).unwrap();
        worktree(&repo_dir, &skipped.join("wt"), "hidden");
        let shown = worktree(&repo_dir, &home.join("wt"), "shown");
        let hidden_repo = repo(&skipped);
        worktree(&hidden_repo, &home.join("other"), "other");
        let repos = BTreeSet::from([repo_dir.join(".git"), hidden_repo.join(".git")]);

        let config = WorktreeConfig::default();
        let now = SystemTime::now();
        let survey = survey(&repos, &Inventory::default(), now, &config, &home);
        let paths: Vec<&Path> = survey
            .worktrees
            .iter()
            .map(|wt| wt.path.as_path())
            .collect();
        assert_eq!(paths, [shown.as_path()]);
    }

    #[test]
    fn surveys_linked_worktrees_and_prunable_records() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = repo(dir.path());
        let wt = worktree(&repo_dir, &dir.path().join("wt"), "feature");
        let gone = worktree(&repo_dir, &dir.path().join("gone"), "gone");
        fs::remove_dir_all(&gone).unwrap();
        let config = WorktreeConfig::default();
        let repos = BTreeSet::from([repo_dir.join(".git")]);

        let now = SystemTime::now();
        let survey = survey(&repos, &Inventory::default(), now, &config, dir.path());
        assert_eq!(survey.worktrees.len(), 1);
        let found = &survey.worktrees[0];
        assert_eq!(found.path, wt);
        assert_eq!(found.branch.as_deref(), Some("refs/heads/feature"));
        assert_eq!(found.repo.dir, repo_dir);
        // Just made: kept, without the slow checks.
        assert_eq!(found.checks, Checks::Pending);
        assert_eq!(survey.prunable.len(), 1);
        assert_eq!(survey.prunable[0].path, gone);

        // A week later the branch, still at main's commit, counts as merged.
        let later = now + 8 * DAY;
        let survey = super::survey(&repos, &Inventory::default(), later, &config, dir.path());
        let found = &survey.worktrees[0];
        assert_eq!(found.done, Some(Done::Merged("refs/heads/main".into())));
        assert_eq!(found.checks, Checks::Passed);
    }

    #[test]
    fn knows_which_unfinished_branches_are_still_on_a_remote() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = repo(dir.path());
        git(&repo_dir, &["remote", "add", "origin", "/nonexistent"]);
        git(
            &repo_dir,
            &["update-ref", "refs/remotes/origin/main", "main"],
        );
        for branch in [
            "tracked",
            "same-name",
            "gone",
            "local",
            "nested",
            "from-main",
        ] {
            let wt = worktree(&repo_dir, &dir.path().join(branch), branch);
            git(&wt, &["commit", "--allow-empty", "-qm", branch]);
        }
        let head = |branch: &str| git(&repo_dir, &["rev-parse", branch]);
        // Pushed with an upstream.
        git(
            &repo_dir,
            &[
                "update-ref",
                "refs/remotes/origin/tracked",
                &head("tracked"),
            ],
        );
        git(
            &repo_dir,
            &[
                "branch",
                "-q",
                "--set-upstream-to=origin/tracked",
                "tracked",
            ],
        );
        // Pushed without one.
        git(
            &repo_dir,
            &[
                "update-ref",
                "refs/remotes/origin/same-name",
                &head("same-name"),
            ],
        );
        // Its upstream deleted, as on merging a pull request.
        git(&repo_dir, &["config", "branch.gone.remote", "origin"]);
        git(
            &repo_dir,
            &["config", "branch.gone.merge", "refs/heads/gone"],
        );
        // Made from origin/main, never pushed.
        git(
            &repo_dir,
            &["branch", "-q", "--set-upstream-to=origin/main", "from-main"],
        );
        // Only a deeper remote branch ends with its name.
        git(
            &repo_dir,
            &[
                "update-ref",
                "refs/remotes/origin/x/nested",
                &head("nested"),
            ],
        );

        let repos = BTreeSet::from([repo_dir.join(".git")]);
        let config = WorktreeConfig::default();
        let later = SystemTime::now() + 8 * DAY;
        let survey = survey(&repos, &Inventory::default(), later, &config, dir.path());
        let state = |branch: &str| {
            let found = survey
                .worktrees
                .iter()
                .find(|wt| wt.branch.as_deref() == Some(&format!("refs/heads/{branch}")))
                .unwrap();
            (found.done.clone(), found.pushed.clone())
        };
        assert_eq!(state("tracked"), (None, Some("origin/tracked".into())));
        assert_eq!(state("same-name"), (None, Some("origin/same-name".into())));
        assert_eq!(
            state("gone"),
            (Some(Done::UpstreamGone("origin/gone".into())), None)
        );
        assert_eq!(state("local"), (None, None));
        assert_eq!(state("nested"), (None, None));
        assert_eq!(state("from-main"), (None, None));
    }

    #[test]
    fn dates_a_worktree_by_its_git_files_and_what_is_built_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = repo(dir.path());
        let wt = worktree(&repo_dir, &dir.path().join("wt"), "feature");
        let admin = git::admin_dir(&wt).unwrap();
        let old = SystemTime::now() - 30 * DAY;
        for path in [
            admin.join("HEAD"),
            admin.join("index"),
            admin.join("logs/HEAD"),
            wt.clone(),
        ] {
            set_mtime(&path, old);
        }
        let (repo, entries) = Repo::list(&repo_dir.join(".git")).unwrap();
        let config = WorktreeConfig::default();
        let now = SystemTime::now();
        let inspect_with =
            |links: &Links| inspect(&repo, &entries[1], links, now, &config, false, || &[]);
        let secs = |time: SystemTime| {
            time.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(
            secs(inspect_with(&Links::default()).last_activity),
            secs(old)
        );

        let built = now - DAY;
        let links = Links {
            output_bases: vec![("/root/base".into(), built)],
            targets: vec![(wt.join("target"), old)],
        };
        assert_eq!(inspect_with(&links).last_activity, built);
    }
}
