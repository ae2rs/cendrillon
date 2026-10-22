//! Finding Bazel caches and Cargo targets: the default locations, paths named in bazelrc files
//! and in Bazel's own records, and the home directory. On macOS, the home directory is walked
//! once and then followed through its change journal; Linux keeps no such journal, so it is
//! walked again every few hours.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::env::Env;
#[cfg(target_os = "macos")]
use crate::fsevents;
use crate::inventory::{Inventory, Item, MARKERS, OutputBase, TRASH_PREFIX, Workspace, classify};
use crate::sys;

/// Deep enough to reach `<root>/<output_user_root>/install/<md5>`.
const ROOT_DEPTH: usize = 3;

/// Bazel flags whose value is a directory Bazel fills with caches.
const PATH_FLAGS: [&str; 5] = [
    "output_user_root",
    "output_base",
    "install_base",
    "disk_cache",
    "repository_cache",
];

/// Directories never walked: they are huge and hold no cache (`.git`, `node_modules`), belong
/// to a tool that manages them (`.rustup` toolchains), hold deleted files (the trash), or
/// reading them makes macOS ask the user for permission. Home directories are given relative
/// to it.
const SKIPPED_NAMES: [&str; 4] = [".git", "node_modules", ".Trash", ".rustup"];
#[cfg(target_os = "macos")]
const SKIPPED_HOME_DIRS: [&str; 8] = [
    "Desktop",
    "Documents",
    "Downloads",
    "Library",
    "Movies",
    "Music",
    "Pictures",
    "Public",
];
#[cfg(target_os = "linux")]
const SKIPPED_HOME_DIRS: [&str; 1] = [".local/share/Trash"];

/// After this long without a replay, the journal may have dropped part of the history since:
/// walk the home directory again instead.
#[cfg(target_os = "macos")]
const JOURNAL_MAX_GAP: Duration = Duration::from_secs(7 * 24 * 3600);
#[cfg(target_os = "macos")]
const REPLAY_TIMEOUT: Duration = Duration::from_secs(300);

/// Without a journal, how long a walk of the home directory stays current. A target that
/// appears in between waits this long to be trimmed, which only matters once it is large.
#[cfg(target_os = "linux")]
const WALK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Where caches were found, kept between runs.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// How far the home directory's change journal was read; `None` until the first walk.
    /// Always `None` on Linux, which keeps no journal.
    pub journal: Option<JournalPosition>,
    /// When the home directory was last walked.
    #[serde(default)]
    pub last_walk: Option<SystemTime>,
    /// Directories to search on every pass, besides the defaults.
    pub roots: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalPosition {
    /// The journal of the home volume. macOS starts a new one when it resets it.
    pub volume: String,
    /// The last event read.
    pub event: u64,
    pub read_at: SystemTime,
}

impl State {
    /// Loads the state, or starts afresh when it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring corrupt state {}: {e}", path.display());
            Self::default()
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("state file has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// Everything Bazel and Cargo keep on disk that cendrillon knows how to find.
pub fn discover(env: &Env, extra_roots: &[PathBuf], state: &State) -> Inventory {
    let mut roots: BTreeSet<PathBuf> = default_roots(env).collect();
    roots.extend(extra_roots.iter().cloned());
    roots.extend(state.roots.iter().filter(|root| !is_skipped(root)).cloned());
    let global_rcs = [
        env.home.join(".bazelrc"),
        PathBuf::from("/etc/bazel.bazelrc"),
    ];
    for rc in &global_rcs {
        roots.extend(bazelrc_paths(rc, None, &env.home));
    }
    let mut items: Vec<Item> = roots
        .iter()
        .flat_map(|root| walk(root, ROOT_DEPTH, &env.home))
        .collect();

    let workspaces: BTreeSet<&Path> = items
        .iter()
        .filter_map(|item| match item {
            Item::OutputBase(OutputBase {
                workspace: Workspace::Present(workspace),
                ..
            }) => Some(workspace.as_path()),
            _ => None,
        })
        .collect();
    let workspace_roots: BTreeSet<PathBuf> = workspaces
        .into_iter()
        .flat_map(|workspace| {
            bazelrc_paths(&workspace.join(".bazelrc"), Some(workspace), &env.home)
        })
        .filter(|root| !roots.contains(root))
        .collect();
    items.extend(
        workspace_roots
            .iter()
            .flat_map(|root| walk(root, ROOT_DEPTH, &env.home)),
    );
    // A deletion of a whole root (a Cargo target) cut short leaves its trash beside it.
    items.extend(
        roots
            .iter()
            .filter(|root| fs::symlink_metadata(root).is_err())
            .flat_map(|root| trash_beside(root)),
    );
    items.into_iter().collect()
}

/// How `refresh` searches the home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSearch {
    Skip,
    /// Replay what changed since last time. Walk instead the first time, or when the journal
    /// lost track. On Linux, walk if the last walk is a few hours old.
    Follow,
    /// Walk it all.
    Walk,
}

/// Learns where caches and targets are, into `state.roots`: from what Bazel recorded, and
/// from the home directory as `home` says. Forgets roots that are gone.
pub fn refresh(env: &Env, extra_roots: &[PathBuf], state: &mut State, home: HomeSearch) {
    let inventory = discover(env, extra_roots, state);
    state
        .roots
        .extend(bazel_records(env, &inventory.output_bases));
    match home {
        HomeSearch::Skip => {}
        HomeSearch::Follow => follow_home(env, state),
        HomeSearch::Walk => walk_home(env, state),
    }
    // `discover` finds the trash of a root deleted midway through the root.
    state
        .roots
        .retain(|root| fs::symlink_metadata(root).is_ok() || !trash_beside(root).is_empty());
}

fn is_skipped(path: &Path) -> bool {
    path.components()
        .any(|part| SKIPPED_NAMES.iter().any(|skip| part.as_os_str() == *skip))
}

/// What renaming `root` to trash produced: `<parent>/.cendrillon-trash-<name>-<stamp>`.
fn trash_beside(root: &Path) -> Vec<Item> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{TRASH_PREFIX}{}-", name.to_string_lossy());
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(&prefix)
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| Item::Trash(entry.path()))
        .collect()
}

/// Where Bazel puts caches unless told otherwise, plus the usual macOS cache directory.
#[cfg(target_os = "macos")]
fn default_roots(env: &Env) -> impl Iterator<Item = PathBuf> {
    [
        PathBuf::from(format!("/private/var/tmp/_bazel_{}", env.user)),
        env.home.join("Library/Caches/bazel"),
        env.home.join(".cache/bazel"),
    ]
    .into_iter()
}

/// Where Bazel puts caches unless told otherwise: `$XDG_CACHE_HOME/bazel`, by default
/// `~/.cache/bazel`. The variable is read when set, but the service may not see it.
#[cfg(target_os = "linux")]
fn default_roots(env: &Env) -> impl Iterator<Item = PathBuf> {
    let xdg = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("bazel"));
    std::iter::once(env.home.join(".cache/bazel")).chain(xdg)
}

/// Walks the home directory and starts following its journal from before the walk.
fn walk_home(env: &Env, state: &mut State) {
    let started = Instant::now();
    #[cfg(target_os = "macos")]
    let (volume, event) = (fsevents::volume(&env.home), fsevents::current_event());
    let found = walk_roots(std::slice::from_ref(&env.home), &env.home);
    tracing::info!(
        "walked the home directory in {:.0?}: {} cache locations",
        started.elapsed(),
        found.len()
    );
    state.roots.extend(found);
    state.last_walk = Some(SystemTime::now());
    #[cfg(target_os = "macos")]
    {
        state.journal = volume.map(|volume| JournalPosition {
            volume,
            event,
            read_at: SystemTime::now(),
        });
    }
}

/// Walks the home directory again once the last walk is a few hours old.
#[cfg(target_os = "linux")]
fn follow_home(env: &Env, state: &mut State) {
    let due = state
        .last_walk
        .is_none_or(|time| time.elapsed().is_ok_and(|age| age >= WALK_INTERVAL));
    if due {
        walk_home(env, state);
    }
}

/// Learns from what changed in the home directory since the last walk or replay.
#[cfg(target_os = "macos")]
fn follow_home(env: &Env, state: &mut State) {
    let Some(volume) = fsevents::volume(&env.home) else {
        tracing::warn!("the home volume keeps no change journal; not searching it");
        return;
    };
    let position = state.journal.as_ref().filter(|position| {
        position.volume == volume
            && position
                .read_at
                .elapsed()
                .is_ok_and(|gap| gap < JOURNAL_MAX_GAP)
    });
    let Some(position) = position else {
        return walk_home(env, state);
    };
    let sealed = sealed_roots(env, state);
    let replayed = fsevents::replay(
        &watched_dirs(&env.home),
        &sealed,
        position.event,
        REPLAY_TIMEOUT,
    );
    let changes = match replayed {
        Ok(changes) if changes.ids_wrapped => return walk_home(env, state),
        Ok(changes) => changes,
        Err(e) => {
            tracing::warn!("replaying changes in the home directory: {e:#}");
            return;
        }
    };
    let changed = changes
        .dirs
        .iter()
        .filter(|dir| !sealed.iter().any(|root| dir.starts_with(root)));
    let mut found = roots_containing(changed, &env.home);
    // The journal lost what happened under these: search them all.
    let lost: BTreeSet<PathBuf> = changes
        .rescan
        .iter()
        .filter(|dir| !is_skipped(dir))
        .map(|dir| match dir.starts_with(&env.home) {
            true => dir.clone(),
            false => env.home.clone(),
        })
        .collect();
    found.extend(walk_roots(&Vec::from_iter(lost), &env.home));
    for root in found.difference(&state.roots) {
        tracing::info!("found {} in the home directory's changes", root.display());
    }
    state.roots.extend(found);
    state.journal = Some(JournalPosition {
        volume,
        event: changes.last_event,
        read_at: SystemTime::now(),
    });
}

/// Where changes are replayed from: every directory under home a walk enters, and nothing it
/// skips, so no protected folder is ever named. `~/Library/Caches` is left out too: it holds
/// Apple's own caches, guarded by privacy checks (media library, Full Disk Access), and the
/// Bazel caches in it are a default root or named in Bazel's records.
#[cfg(target_os = "macos")]
fn watched_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(device) = fs::metadata(home).map(|meta| meta.dev()) else {
        return Vec::new();
    };
    let entries = fs::read_dir(home)
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    subdirs(home, entries, home, device)
}

/// Known roots nothing new can appear in unseen, so their changes need no replay: Bazel's
/// default roots, searched on every pass, and the caches and targets themselves. Nested ones
/// are left out.
#[cfg(target_os = "macos")]
fn sealed_roots(env: &Env, state: &State) -> Vec<PathBuf> {
    let is_item = |root: &PathBuf| classify(root).is_some_and(|item| item.path() == root);
    let candidates: BTreeSet<PathBuf> = default_roots(env)
        .filter(|root| root.is_dir())
        .chain(state.roots.iter().filter(|root| is_item(root)).cloned())
        .collect();
    let mut sealed: Vec<PathBuf> = Vec::new();
    for root in candidates {
        if !sealed.iter().any(|outer| root.starts_with(outer)) {
            sealed.push(root);
        }
    }
    sealed
}

/// Search roots of the caches and targets holding `dirs`, directories changed under `home`.
#[cfg(target_os = "macos")]
fn roots_containing<'a>(
    dirs: impl IntoIterator<Item = &'a PathBuf>,
    home: &Path,
) -> BTreeSet<PathBuf> {
    let mut checked = HashSet::new();
    let mut roots = BTreeSet::new();
    for dir in dirs.into_iter().filter(|dir| !is_skipped(dir)) {
        let ancestors = dir
            .ancestors()
            .take_while(|ancestor| *ancestor != home && ancestor.starts_with(home));
        for ancestor in ancestors {
            // Its own ancestors were checked with it.
            if !checked.insert(ancestor.to_path_buf()) {
                break;
            }
            if let Some(item) = classify(ancestor) {
                roots.extend(search_root(&item));
                break;
            }
        }
    }
    roots
}

/// Search roots of the caches and targets found walking `dirs` all the way down.
fn walk_roots(dirs: &[PathBuf], home: &Path) -> BTreeSet<PathBuf> {
    dirs.iter()
        .flat_map(|dir| walk(dir, usize::MAX, home))
        .filter_map(|item| search_root(&item))
        .collect()
}

/// The directory to search to find `item` again, along with its siblings: the output user
/// root for output bases and install bases, the directory itself for caches and targets.
fn search_root(item: &Item) -> Option<PathBuf> {
    match item {
        Item::OutputBase(_) | Item::Trash(_) => item.path().parent().map(Path::to_path_buf),
        Item::InstallBase(_) => item.path().parent()?.parent().map(Path::to_path_buf),
        Item::Cache(cache) => Some(cache.root.clone()),
        Item::CargoTarget(target) => Some(target.root.clone()),
    }
}

/// Classifies directories under `root`, not descending into what it recognizes, nor into
/// other volumes.
fn walk(root: &Path, max_depth: usize, home: &Path) -> Vec<Item> {
    let Ok(root) = fs::canonicalize(root) else {
        return Vec::new();
    };
    let Ok(device) = fs::metadata(&root).map(|meta| meta.dev()) else {
        return Vec::new();
    };
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut items = Vec::new();
    let mut pending = vec![(root, 0)];
    while let Some((dir, depth)) = pending.pop() {
        // Listed once, both to skip `classify` where no marker is, which spares most of the
        // system calls of a walk, and to find the subdirectories.
        let entries: Option<Vec<fs::DirEntry>> = fs::read_dir(&dir)
            .ok()
            .map(|entries| entries.filter_map(Result::ok).collect());
        let may_be_item = entries.as_ref().is_none_or(|entries| {
            entries
                .iter()
                .any(|entry| MARKERS.iter().any(|marker| entry.file_name() == *marker))
        }) || dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(TRASH_PREFIX));
        if let Some(item) = may_be_item.then(|| classify(&dir)).flatten() {
            items.push(item);
        } else if depth < max_depth {
            let subdirs = subdirs(&dir, entries.unwrap_or_default(), &home, device);
            pending.extend(subdirs.into_iter().map(|subdir| (subdir, depth + 1)));
        }
    }
    items
}

/// The directories in `dir`, listed as `entries`, that a search enters: real directories on
/// `device`, but not the skipped ones.
///
/// A skipped directory is never opened, not even listed: reading a protected folder such as
/// `~/Documents` is what makes macOS ask the user for permission. Walkers like `walkdir`
/// list a directory before letting the caller skip it, hence this hand-written recursion.
fn subdirs(dir: &Path, entries: Vec<fs::DirEntry>, home: &Path, device: u64) -> Vec<PathBuf> {
    entries
        .into_iter()
        .filter(|entry| {
            let name = entry.file_name();
            // From the directory listing itself: no symlink is followed, nothing is opened.
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && !SKIPPED_NAMES.iter().any(|skip| name == *skip)
                && !dir.join(&name).strip_prefix(home).is_ok_and(|relative| {
                    SKIPPED_HOME_DIRS
                        .iter()
                        .any(|skip| relative == Path::new(skip))
                })
                && entry.metadata().is_ok_and(|meta| meta.dev() == device)
        })
        .map(|entry| entry.path())
        .collect()
}

/// Paths Bazel was given on the command line, which no bazelrc shows: in the command lines of
/// running Bazel servers, and in the commands each output base logged to its `java.log`.
fn bazel_records(env: &Env, output_bases: &[OutputBase]) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    for command in sys::command_lines() {
        paths.extend(logged_paths(&command, &[' ', '\n'], None, &env.home));
    }
    for base in output_bases {
        let workspace = match &base.workspace {
            Workspace::Present(path) => Some(path.as_path()),
            _ => None,
        };
        // `java.log` links to the latest of the logs.
        let logs = fs::read_dir(&base.path)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with("java.log")
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
            });
        for log in logs {
            if let Ok(text) = fs::read(log.path()) {
                let text = String::from_utf8_lossy(&text);
                paths.extend(logged_paths(&text, &[',', ']', '\n'], workspace, &env.home));
            }
        }
    }
    paths
}

/// Values of `--<path flag>=<value>` arguments in `text`, each value running to one of `ends`.
fn logged_paths(text: &str, ends: &[char], workspace: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for flag in PATH_FLAGS {
        let prefix = format!("--{flag}=");
        for (at, _) in text.match_indices(&prefix) {
            let value = text[at + prefix.len()..]
                .split(ends)
                .next()
                .unwrap_or_default();
            paths.extend(expand(value, workspace, home));
        }
    }
    paths
}

/// Cache directories named by path flags in a bazelrc file and the files it imports. Flags
/// under every `--config` count, since any of them may have been used.
fn bazelrc_paths(file: &Path, workspace: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut visited = HashSet::new();
    collect_bazelrc_paths(file, workspace, home, &mut visited, &mut paths);
    paths
}

fn collect_bazelrc_paths(
    file: &Path,
    workspace: Option<&Path>,
    home: &Path,
    visited: &mut HashSet<PathBuf>,
    paths: &mut Vec<PathBuf>,
) {
    if !visited.insert(file.to_path_buf()) {
        return;
    }
    let Ok(text) = fs::read_to_string(file) else {
        return;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        let mut words = line.split_whitespace();
        let Some(command) = words.next() else {
            continue;
        };
        if command == "import" || command == "try-import" {
            if let Some(imported) = words.next().and_then(|w| expand(w, workspace, home)) {
                collect_bazelrc_paths(&imported, workspace, home, visited, paths);
            }
            continue;
        }
        let args: Vec<&str> = words.collect();
        for (i, arg) in args.iter().enumerate() {
            let Some(flag) = arg.strip_prefix("--") else {
                continue;
            };
            let (name, value) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value).filter(|v| !v.is_empty())),
                None => (
                    flag,
                    args.get(i + 1).copied().filter(|v| !v.starts_with("--")),
                ),
            };
            if PATH_FLAGS.contains(&name)
                && let Some(path) = value.and_then(|v| expand(v, workspace, home))
            {
                paths.push(path);
            }
        }
    }
}

/// Resolves `~/` and `%workspace%` as Bazel does. Relative paths are dropped: they depend on
/// the directory Bazel ran in.
fn expand(value: &str, workspace: Option<&Path>, home: &Path) -> Option<PathBuf> {
    let value = value.trim_matches(['"', '\'']);
    let path = if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if let Some(rest) = value.strip_prefix("%workspace%") {
        workspace?.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(value)
    };
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::fixtures::{disk_cache, output_base};

    fn env(dir: &Path) -> Env {
        Env {
            home: fs::canonicalize(dir).unwrap(),
            user: "cendrillon-test-user".into(),
        }
    }

    #[test]
    fn parses_path_flags_from_bazelrc_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let workspace = home.join("repo");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            workspace.join(".bazelrc"),
            "common:macos --disk_cache=~/Library/Caches/bazel/_bazel_disk_cache\n\
             common:linux --disk_cache=~/.cache/bazel/_bazel_disk_cache # comment\n\
             common:ci --disk_cache=\n\
             startup --output_user_root /tmp/bazel-root\n\
             build --repository_cache=relative/path --jobs=8\n\
             try-import %workspace%/user.bazelrc\n",
        )
        .unwrap();
        fs::write(
            workspace.join("user.bazelrc"),
            "build --disk_cache=/big/cache\n",
        )
        .unwrap();

        let paths = bazelrc_paths(&workspace.join(".bazelrc"), Some(&workspace), home);
        assert_eq!(
            paths,
            [
                home.join("Library/Caches/bazel/_bazel_disk_cache"),
                home.join(".cache/bazel/_bazel_disk_cache"),
                PathBuf::from("/tmp/bazel-root"),
                PathBuf::from("/big/cache"),
            ]
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn discovers_default_root_and_caches_named_by_workspace_bazelrc() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let workspace = env.home.join("work/repo");
        fs::create_dir_all(&workspace).unwrap();
        let elsewhere = env.home.join("elsewhere/disk");
        fs::write(
            workspace.join(".bazelrc"),
            format!("build --disk_cache={}\n", elsewhere.display()),
        )
        .unwrap();
        let user_root = env.home.join("Library/Caches/bazel/_bazel_test");
        let now = SystemTime::now();
        output_base(&user_root, "aaaa", &workspace, now);
        let install = user_root.join("install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("build-label.txt"), "9.2.0").unwrap();
        disk_cache(&elsewhere, &[]);

        let inventory = discover(&env, &[], &State::default());
        assert_eq!(inventory.output_bases.len(), 1);
        assert_eq!(inventory.install_bases.len(), 1);
        assert_eq!(inventory.caches.len(), 1);
        assert_eq!(inventory.caches[0].root, elsewhere);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn home_scan_finds_caches_and_skips_protected_folders() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let now = SystemTime::now();
        output_base(
            &env.home.join("odd/place/root"),
            "bbbb",
            Path::new("/x"),
            now,
        );
        disk_cache(&env.home.join("Documents/cache"), &[]);
        disk_cache(&env.home.join("src/node_modules/cache"), &[]);
        disk_cache(&env.home.join("Library/Caches/com.apple.Music/disk"), &[]);
        disk_cache(&env.home.join("Library/Developer/disk"), &[]);
        let bazel = disk_cache(&env.home.join("Library/Caches/bazel/disk"), &[]);

        let roots = walk_roots(std::slice::from_ref(&env.home), &env.home);
        assert_eq!(roots, BTreeSet::from([env.home.join("odd/place/root")]));
        assert!(
            !watched_dirs(&env.home)
                .iter()
                .any(|dir| dir.starts_with(env.home.join("Library")))
        );

        let state = State {
            journal: None,
            last_walk: None,
            roots,
        };
        let inventory = discover(&env, &[], &state);
        assert_eq!(inventory.output_bases.len(), 1);
        // Bazel's folder in `~/Library/Caches` is a default root.
        assert_eq!(inventory.caches.len(), 1);
        assert_eq!(inventory.caches[0].root, bazel);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn discovers_default_root_and_caches_named_by_workspace_bazelrc() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let workspace = env.home.join("work/repo");
        fs::create_dir_all(&workspace).unwrap();
        let elsewhere = env.home.join("elsewhere/disk");
        fs::write(
            workspace.join(".bazelrc"),
            format!("build --disk_cache={}\n", elsewhere.display()),
        )
        .unwrap();
        let user_root = env.home.join(".cache/bazel/_bazel_test");
        let now = SystemTime::now();
        output_base(&user_root, "aaaa", &workspace, now);
        let install = user_root.join("install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("build-label.txt"), "9.2.0").unwrap();
        disk_cache(&elsewhere, &[]);

        let inventory = discover(&env, &[], &State::default());
        assert_eq!(inventory.output_bases.len(), 1);
        assert_eq!(inventory.install_bases.len(), 1);
        assert_eq!(inventory.caches.len(), 1);
        assert_eq!(inventory.caches[0].root, elsewhere);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn home_scan_finds_caches_and_skips_the_trash() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let now = SystemTime::now();
        output_base(
            &env.home.join("odd/place/root"),
            "bbbb",
            Path::new("/x"),
            now,
        );
        disk_cache(&env.home.join(".local/share/Trash/files/disk"), &[]);
        disk_cache(&env.home.join("src/node_modules/cache"), &[]);
        let documents = disk_cache(&env.home.join("Documents/cache"), &[]);

        let roots = walk_roots(std::slice::from_ref(&env.home), &env.home);
        assert_eq!(
            roots,
            BTreeSet::from([env.home.join("odd/place/root"), documents])
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn walks_the_home_directory_again_once_the_last_walk_is_old() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let mut state = State::default();
        follow_home(&env, &mut state);
        let walked = state.last_walk.unwrap();
        assert!(state.roots.is_empty());

        let cache = disk_cache(&env.home.join("new/a/disk"), &[]);
        follow_home(&env, &mut state);
        assert_eq!(state.last_walk, Some(walked));
        assert!(state.roots.is_empty());

        state.last_walk = Some(walked - WALK_INTERVAL);
        follow_home(&env, &mut state);
        assert_eq!(state.roots, BTreeSet::from([cache]));
    }

    #[test]
    fn a_walk_finds_every_kind_classify_recognizes() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let now = SystemTime::now();
        output_base(&env.home.join("a/root"), "cccc", Path::new("/x"), now);
        let install = env
            .home
            .join("b/root/install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("build-label.txt"), "9.2.0").unwrap();
        let disk = disk_cache(&env.home.join("c/disk"), &[]);
        let repos = env.home.join("d/repos");
        fs::create_dir_all(repos.join("content_addressable/sha256")).unwrap();
        let target = env.home.join("e/target");
        fs::create_dir_all(target.join("debug/.fingerprint")).unwrap();
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        fs::write(target.join(".rustc_info.json"), "{}").unwrap();
        let trash = env.home.join(format!("f/{TRASH_PREFIX}x-1"));
        fs::create_dir_all(&trash).unwrap();

        let found: BTreeSet<PathBuf> = walk(&env.home, usize::MAX, &env.home)
            .iter()
            .map(|item| item.path().to_path_buf())
            .collect();
        assert_eq!(
            found,
            BTreeSet::from([
                env.home.join("a/root/cccc"),
                install,
                disk,
                repos,
                target,
                trash
            ])
        );
    }

    #[test]
    fn home_scan_follows_no_symlink_and_survives_unreadable_skipped_folders() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let outside = tempfile::tempdir().unwrap();
        disk_cache(&outside.path().join("disk"), &[]);
        std::os::unix::fs::symlink(outside.path(), env.home.join("link")).unwrap();
        let documents = env.home.join("Documents");
        disk_cache(&documents.join("cache"), &[]);
        let mode = |m| std::os::unix::fs::PermissionsExt::from_mode(m);
        fs::set_permissions(&documents, mode(0o000)).unwrap();
        disk_cache(&env.home.join("src/a/b/c/disk"), &[]);

        let roots = walk_roots(std::slice::from_ref(&env.home), &env.home);
        fs::set_permissions(&documents, mode(0o755)).unwrap();
        assert_eq!(roots, BTreeSet::from([env.home.join("src/a/b/c/disk")]));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_changed_directory_leads_to_the_cache_holding_it() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let cache = disk_cache(&env.home.join("src/a/disk"), &[]);
        disk_cache(&env.home.join("src/node_modules/disk"), &[]);
        fs::create_dir_all(env.home.join("src/other")).unwrap();
        let changed = [
            cache.join("ac/00"),
            cache.join("cas/00"),
            env.home.join("src/node_modules/disk/ac/00"),
            env.home.join("src/other"),
            env.home.join("src/deleted"),
        ];
        assert_eq!(
            roots_containing(&changed, &env.home),
            BTreeSet::from([cache])
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn follows_the_home_directory_through_its_journal() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let mut state = State::default();
        walk_home(&env, &mut state);
        assert!(state.journal.is_some());
        assert!(state.roots.is_empty());

        let cache = disk_cache(&env.home.join("new/a/disk"), &[]);
        // Events reach the journal a little late.
        let deadline = Instant::now() + Duration::from_secs(120);
        while !state.roots.contains(&cache) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            follow_home(&env, &mut state);
        }
        assert_eq!(state.roots, BTreeSet::from([cache]));
    }

    #[test]
    fn refresh_forgets_roots_that_are_gone_but_not_those_left_in_trash() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let cache = disk_cache(&env.home.join("src/disk"), &[]);
        let target = env.home.join("src/repo/target");
        fs::create_dir_all(env.home.join("src/repo/.cendrillon-trash-target-1700")).unwrap();
        let gone = env.home.join("src/gone");
        let mut state = State {
            journal: None,
            last_walk: None,
            roots: BTreeSet::from([cache.clone(), target.clone(), gone.clone()]),
        };
        refresh(&env, &[], &mut state, HomeSearch::Skip);
        assert!(state.roots.contains(&cache));
        assert!(state.roots.contains(&target));
        assert!(!state.roots.contains(&gone));
    }

    #[test]
    fn reads_path_flags_from_server_command_lines_and_logged_commands() {
        let home = Path::new("/home");
        let ps = "/usr/bin/zsh\n\
                  bazel(repo) -jar A-server.jar --output_user_root=/root \
                  --output_base=/root/abcd --install_base=/root/install/ef\n";
        assert_eq!(
            logged_paths(ps, &[' ', '\n'], None, home),
            [
                PathBuf::from("/root"),
                PathBuf::from("/root/abcd"),
                PathBuf::from("/root/install/ef"),
            ]
        );
        let log = "261020 07:22:57.714:I 53 [GrpcServerImpl.executeCommand] [build, \
                   --disk_cache=, --disk_cache=~/disk, --experimental_disk_cache_gc_max_size=1, \
                   --repository_cache=%workspace%/repos]\n";
        assert_eq!(
            logged_paths(log, &[',', ']', '\n'], Some(Path::new("/ws")), home),
            [PathBuf::from("/home/disk"), PathBuf::from("/ws/repos")]
        );
    }

    #[test]
    fn finds_the_trash_of_a_root_deleted_midway() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let trash = env.home.join("src/repo/.cendrillon-trash-target-1700");
        fs::create_dir_all(trash.join("debug")).unwrap();
        let state = State {
            journal: None,
            last_walk: None,
            roots: BTreeSet::from([env.home.join("src/repo/target")]),
        };
        assert_eq!(discover(&env, &[], &state).trash, [trash]);
    }

    #[test]
    fn state_round_trips_and_tolerates_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.json");
        assert_eq!(State::load(&path), State::default());
        let state = State {
            journal: Some(JournalPosition {
                volume: "0123".into(),
                event: 7,
                read_at: SystemTime::UNIX_EPOCH,
            }),
            last_walk: Some(SystemTime::UNIX_EPOCH),
            roots: BTreeSet::from([PathBuf::from("/a")]),
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);
        fs::write(&path, "{").unwrap();
        assert_eq!(State::load(&path), State::default());
        // Written before the journal was followed: the roots stay, and a walk comes first.
        fs::write(&path, r#"{"last_home_scan": null, "roots": ["/a"]}"#).unwrap();
        assert_eq!(
            State::load(&path),
            State {
                journal: None,
                last_walk: None,
                roots: BTreeSet::from([PathBuf::from("/a")]),
            }
        );
    }
}
